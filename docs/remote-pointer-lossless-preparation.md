# Native pointer-only plus static lossless repair

The v27 baseline and its validation history are recorded below. The v28 vertical-scroll baseline is validated below. The final section
describes the separate v29 horizontal-scroll combination under development;
its new permission combination has not yet been validated in CI.

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
strictly zero-action. These distinctions were checked by the successful follow-up candidate run described below.
The updated module has six real fixtures, including 800×600 multi-batch repair,
and one pure marker negative test. The follow-up passes strict Clippy, test
compilation, 39 pure phase/client/host/wire regressions, the marker negative test
and the arithmetic checks below. Candidate `9b3105` then passed
[general CI](https://github.com/beamiter/jwm/actions/runs/42939234419)
(8,267 passed, 37 ignored),
[Wayland CI](https://github.com/beamiter/jwm/actions/runs/42939234552)
(remote suite 1,009 passed, 30 ignored; CLI 53 passed), and X11 CI.
All seven tests in the baseline module passed. All four baseline mainline
workflows subsequently passed:
[general](https://github.com/beamiter/jwm/actions/runs/42941742193)
(8,267 passed, 37 ignored),
[X11](https://github.com/beamiter/jwm/actions/runs/42941742100),
[Wayland](https://github.com/beamiter/jwm/actions/runs/42941742204)
(remote 1,009 passed, 30 ignored), and
[Eva](https://github.com/beamiter/jwm/actions/runs/42941742186).
These are owned software-GL results, not physical hardware or latency acceptance.
The separate natural-expiry candidate below remains unvalidated.

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
the positive pointer combination and mixed-mode/profile negative cases.
Baseline real integration CI also passed as recorded above.

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
convergence. The 800×600 real fixture completed eight batches to whole-frame
RGB SSE=0 in three executions: 18,371 / 18,360 / 18,449 ms, followed by an actual
click through the renewed proof/presentation gate, without renewing the lease.
The small fixture took 2,214 / 2,218 / 2,225 ms. The mainline repeat
completed the 800×600 eight-batch exact repair in 18,388 / 18,289 / 18,413 ms,
with two actual clicks in each fixture execution. These are observed synthetic
software-GL convergence samples, not upper bounds or hardware timings. They
validate multi-batch progress at 800×600 but do not remove the 1080p budget
limitation below.

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
its constants aligned if the implementation changes. The 800×600 real test now covers multi-batch exactness. Further representative
coverage should check partial progress/terminal expiry at 1080p, without demanding impossible 1080p full convergence or changing
the 30-second deadline.


## Next candidate: natural expiry followed by read-only repair

A separate candidate is in progress to handle natural pointer-lease expiry as
an irreversible transition to read-only viewing and repair, and to add a real
1080p partial-progress/expiry regression. The input lease remains 30 seconds,
with no renewal or restoration of input authority. Any exact convergence after
expiry belongs to read-only operation, not to the active pointer-lease budget
in the table above.

Inspection confirmed a client-side misclassification at the natural deadline.
An initial suspicion that host capture remained frozen was disproved by the
complete stop path: it already clears `pointer_enabled`. The actual host issue
under repair is a late-flush race. This distinction matters: the candidate must
not be described as fixing a confirmed capture-freeze defect or as validated
merely because ordinary stop already changes the flag.

The expanded module contains seven real fixtures and two pure tests, nine in
total. Its unsuccessful candidate runs are recorded below. The successful
`9b3105` results above describe the baseline only; they do not validate these
new client/host changes or the 1080p expiry path. Real tests must establish
partial progress before expiry, permanent loss of input authority, safe
late-record handling, and continuing read-only repair without automatic
regrant. Local strict Clippy, 50 pure sequencing/host/client/wire checks,
the exact-expiry ACK negative test, formatting and arithmetic checks pass.
Independent static review found no remaining blocker. These checks are not
execution of the new real protocol/GL test or physical-hardware acceptance.
Terminal handling drains only bounded, exactly identified in-flight evidence;
unmatched messages still fail closed, so arbitrary stalls are not promised to
preserve the video session.

The first new-candidate run (`ea4b7648`) passed eight module checks but failed
the 1080p fixture during initialization: the shared test SHM helper still had
its original 800×600 capacity. It did not reach the new lease-expiry assertions.
A fixed-size owned 1920×1080 test constructor now checks its stride and byte
allocation explicitly; all old constructors retain their original limits. This
is a test-infrastructure correction, not a production-size-limit change. The
new real path still requires a successful rerun before acceptance.


The second run (`3057fe0c`) again passed eight module checks and failed one.
The 1080p fixture completed its initial GL frame but had not obtained an input
grant. It failed on the host freshness precondition path, before the intended
expiry assertions. The observed capture-start-to-ACK interval was approximately
1.52 seconds; that interval is not the exact age from native proof readiness
to the lease request and does not establish a unique error message or sole
cause. This run does not validate the expiry behavior.

The next candidate keeps the same 1080p test body, one-second proof freshness,
30-second lease and all assertions. It schedules this test in a mandatory
independent job using the existing release-grade profiling build. The ordinary
debug suite explicitly reports this one test as ignored; the mandatory job
must select it with `--ignored --exact` and verify **one passed, zero ignored**.
The debug ignore is not counted as a passing test or a substitute for that job.

The initial full-frame CPU oracle is moved after actual presentation feedback,
while retaining its complete pixel checks. There is no fabricated presentation
timestamp or relaxed timing gate. Results for this revised CI execution are
still pending; this is neither a verified performance fix nor physical-hardware
acceptance. The successful `9b3105` baseline remains separate from this work.


The mandatory optimized run for `f35d24ee` did execute the 1080p test. It
obtained the real native grant and completed eight repair batches, increasing
exact pixels from 65,536 to 524,288 while full-image SSE decreased from
121,125,364 to 93,749,548. The test still failed after about 20.68 seconds,
before natural expiry, when an ordinary Ready reached the client before its
local presentation bookkeeping. This partial progress is not a passing expiry
result. General and X11 CI and the ordinary Wayland job passed separately.

The follow-up fixes the ordering at the actual presentation callback: record
the exact NativeProof receipt before publishing its ACK, while keeping the
initial input request after ACK. A bounded exact-receipt history retires a late
old Ready without authorizing an older frame, and later decoded frames remain
invalidating even if their predecessor's feedback is processed afterward.
Neither Video nor PNG repair creates this receipt. In the active flow, Ready
requires exact actual-presentation evidence and Ready replay is rejected. The
same mandatory optimized test must pass before
this candidate can be accepted; no safety deadline is increased.
Local strict Clippy and 62 focused pure regressions pass for this ordering
follow-up, including the actual-receipt/ACK race, newer-frame invalidation,
exact old Ready retirement, replay rejection and existing pointer/scroll/repair
cases. This is preparation for the mandatory real rerun, not its result.


## Validated natural-expiry baseline (`c16db981`)

The subsequent `c16db981` baseline passed all four mainline CI workflows,
including the mandatory optimized 1080p test. The actual 1080p run completed
14 repair cycles and observed natural expiry at 30.166 seconds, with remaining
full-image SSE 68,789,192. This is partial progress followed by expiry, not
whole-frame exactness within the input lease. The observed expiry timestamp is
a test observation, not permission to submit input beyond the 30-second lease.

This completes the real rerun required by the earlier failed candidates above;
their failures remain part of the history. The test establishes the safe
transition to read-only operation and does not renew input authority. It does
not alter the 32-batch / 64-second ideal 1080p bound during active pointer mode.
All results are from owned software-GL CI, not physical display or hardware
latency acceptance.

## Next candidate: explicit vertical discrete scroll with PNG (v28)

The next candidate permits the existing native pointer-only PNG mode to request
vertical wheel input separately on both peers:

```sh
jwm-remote host --wayland-select --once --allow-wayland-pointer --allow-static-lossless --allow-wayland-scroll
jwm-remote connect HOST --viewer-backend wayland --wayland-pointer --static-lossless --wayland-scroll
```

The CLI accepts this combination and retains the native Wayland viewer
requirement. Neither static lossless nor pointer permission implies scroll
permission. Horizontal scroll, unified control, keyboard, text, clipboard,
input grab, popup capture and resize-follow remain excluded from this
combination. View-only is not added to this command. Reconnect and saved-profile
authorization remain prohibited. Existing separate modes do not silently gain
permissions.

Application v28 makes the changed combination contract explicit. Both endpoints
must match; v27 and other application versions are rejected without fallback.
The existing capability fields and scroll action encoding are reused. Transport
v3 and native protocol versions do not change merely to enable this combination.

The intended interaction remains single-axis discrete vertical Wheel input,
one notch per fresh native proof and actual presentation. Repair feedback
cannot create a scroll ticket. Input during repair or Waiting must not be
queued for replay when Ready returns. Action completion receipts, phase
cancellation, held-input rejection and natural-expiry terminal handling all
need to cover scroll as well as motion/click. The original 30-second lease,
repair budget and cooldown remain unchanged.

The CLI/version changes and new pure positive/negative cases are implemented;
centralized execution and real combined recipient/PNG/presentation tests are
pending. The `c16db981` green results validate the prior baseline only, not this
new combination. Required evidence includes actual up/down recipient events
and changed captured content, new proof after repair, no replay of full or
partial axis groups across Waiting, denial of malicious unnegotiated scroll,
and permanent input revocation while read-only repair can continue. No v28
functional success, performance improvement or hardware acceptance is claimed.

The vertical-scroll candidate passes local compilation, strict Clippy and 106
pure regressions across repair ordering, viewer pointer grouping, explicit host
policy, CLI validation and versioned messages. One new test initially tried to
reuse a fence after a deliberately invalid receipt; it was corrected to prove
that the invalid session remains terminal, without changing production logic.
The six new real display/TCP cases and two malicious-record scenarios still
require CI; local pure checks are not recipient or pixel acceptance.

The first vertical-scroll CI candidate exposed a remaining wire capability
validation rule: all six new combinations were rejected during Hello decoding,
before native input execution. General CI had 8,289 passing and six failing
tests; the dedicated module had eight passing, six failing and its one separately
required optimized test. X11 and the mandatory optimized 1080p expiry case passed,
but this does not accept the scroll combination. The follow-up permits only the
vertical pointer-only combination in that shared decoder and adds actual client
and server Hello round trips, a 64-combination permission matrix, and malformed
flag/version negatives. Production native authorization is not relaxed.


## Validated vertical-scroll baseline (`fa8`)

The `fa8` mainline passed all four CI workflows. Three 800×600 executions
completed eight repair batches to full-image SSE=0 and delivered two actual
notches, taking 17,980 / 17,976 / 18,009 ms. The mandatory optimized 1080p run
completed 13 cycles and observed expiry at 30,150 ms, with remaining SSE
73,065,117. These are owned software-GL observations, not physical-hardware
acceptance or a general latency bound. The 1080p result remains partial; input
authority is not extended to finish all repair debt.

## Validated baseline: independent horizontal scroll with PNG (v29)

This combination was validated with application v29 with strict matching peers. Older
v28 endpoints are rejected without downgrade. Transport v3, capability payload
size and the existing native action encodings remain unchanged. Horizontal
scroll is a separate explicit permission at both endpoints; vertical consent
does not grant it, and horizontal-only use does not require vertical consent:

```sh
jwm-remote host --wayland-select --once --allow-wayland-pointer --allow-static-lossless --allow-wayland-horizontal-scroll
jwm-remote connect HOST --viewer-backend wayland --wayland-pointer --static-lossless --wayland-horizontal-scroll
```

To authorize both directions, also add `--allow-wayland-scroll` on the host and
`--wayland-scroll` on the viewer. The two permissions allow separate single-axis
notches, never mixed axes in one actual pointer frame. No Shift translation,
smooth scrolling, multinotch batching, drag or modifier-click is added.
Keyboard, unified control, text, legacy input, clipboard, popup capture and
resize combinations remain prohibited. The native Wayland viewer requirement,
no reconnect/profile authorization, one-notch-per-fresh-proof rule and original
30-second nonrenewable lease remain in force. SSH mode does not acquire these
new CLI permissions.

The shared capability decoder now accepts axis bits 0, 1, 2 or 3 for the
PNG/pointer mode, while unknown bits and scroll without the required native
pointer mode remain invalid. Pure regression cases exercise actual client and
server Hello encoding/decoding, all 64 native-mode/axis combinations,
horizontal-only and both-axis round trips, old/future version rejection,
legacy/clipboard/resize restrictions, and CLI/profile/SSH guards. These cases subsequently passed the `02f` validation below. The successful
`fa8` results describe the prior vertical-scroll baseline; neither result
constitutes physical-hardware acceptance.

The horizontal candidate passes local strict Clippy and 118 pure regressions,
including all message capability sibling modules and both actual Hello decoders.
Independent static review found no remaining contract mismatch. The combined
fixture list now contains 22 tests, including the unchanged independently required
optimized 1080p test; seven new horizontal cases and two additional malicious raw
records were included in the subsequent CI validation. No local display, socket or renderer acceptance is claimed.

The first horizontal candidate passed general CI (8,308 tests), X11, the new
real horizontal scenarios and the mandatory optimized 1080p case. The final
Wayland aggregate still failed an existing read-only blink test: one generation
missed its unchanged 500 ms actual-presentation requirement, with 1,049 other
tests passing. It was not merged. Source repaint was about 160 ms late while the
same harness thread spent about 100 ms checking each full frame. The horizontal
production diff did not change that read-only capture or scheduling path.

A test-only follow-up computes the identical full static-region RGBA SSE in row
spans, preserving every channel and the exact 16×16 exclusion. Equal spans prove
zero error directly. A debug pure-CPU alternating A/B (two warmups, eight samples)
measured reference/new median costs of 64.075/0.096 ms for equal frames,
61.333/10.971 ms for half-frame differences and 61.379/21.693 ms for dense
differences, with identical SSE in each case. Boundary, alpha, last-pixel and
maximum-error reference comparisons cover the oracle. Fixed cadence, 500 ms,
18-second total deadline, actual presentation timestamps and production code are
unchanged. This demonstrates fixture CPU cost, not the sole cause of the missed
deadline or a production latency improvement.

The follow-up `02f` candidate passed CI and was fast-forwarded to master. All
four mainline reruns then passed: general 42970624553, X11 42970624415,
Wayland 42970624389 and Eva 42970624410. Remote tests passed 1,051 and CLI
tests 55; all three mainline blink executions retained and met the original
deadline. These results validate the v29 baseline in owned software-GL tests,
not the new v30 unified-control repair combination or physical hardware.

## Unified control plus PNG: v30 candidate

The next explicit combination uses host `--wayland-select --once --allow-wayland-control --allow-static-lossless` and viewer `connect HOST --viewer-backend wayland --wayland-control --static-lossless`. Both ends must match application v30; transport v3 is unchanged. This is a single unified lease, not simultaneous keyboard-only and pointer-only leases. Existing pointer-only independently authorized scroll plus PNG remains supported.

This first unified repair slice rejects both scroll axes, legacy input, separate keyboard/pointer flags, text, clipboard, resize/follow, popup capture, saved profiles and SSH authorization forwarding. The original 30-second lease is not renewed. Repair requires released keys, no click candidate and completed actions; repair presentation cannot grant input. Resuming input requires a new native proof and its actual presentation. Control completion and repair messages use distinct kinds 44–48; sharing a bounded payload layout does not share pointer authorization.

For this first unified-control slice, natural expiry of the original 30-second lease may close the entire session. Continued read-only repair after expiry, as supported by pointer-only plus PNG, is not promised here. Existing held-key and repair-phase short deadlines remain separate failure conditions; reaching one does not demonstrate natural lease expiry. Locking or local takeover still revokes authority immediately.

Real unified-control repair CI has not fully passed yet; prior pointer-only software-GL results do not validate this new combination or physical hardware.

The first v30 candidate passes strict Clippy and 147 local pure regressions for
control completion/repair, existing pointer repair, the complete Hello capability
matrix, non-socket transport records, viewer busy state and CLI policy. The two
socket-based handshake tests are reserved for CI, not included in that local
count. Independent static review checks distinct native proof ownership, bounded
32-action completion order, finite terminal receipts and actual presentation
before ACK. Six real owned-display/TCP scenarios cover: 800×600 click/key/release/repair/click, held release,
Accepted input rejection, denied host permission, lock/unmap ABA, and the original
30-second lease deadline. Local pure results do not substitute for these checks.

The first real `304a` combination run passed two of six scenarios. Three fixtures
withheld an earlier NativeProof before repair acceptance, so they could not reach
the intended Accepted/5-second test stage. The TTL scenario also failed when a
generic terminal error obscured its expiry classification. The general full suite
passed 8,330 tests with three failures and 38 ignored: the same three fixtures
failed there, while TTL passed. X11 and mandatory optimized 1080p passed.

The pending rerun keeps all assertions and deadlines. The fixture correction
holds only the exact VisualRepair sequence, rather than a prerequisite native
proof. The terminal correction freezes the earlier applicable deadline in its
receipt and gives expiry classification precedence only when local receipt time
has already reached that deadline. That classification alone does not prove
natural expiry: the real TTL test separately checks the original native and
client deadlines and a single lease. These corrections await rerun; no complete
v30 acceptance or physical-hardware validation is claimed.
