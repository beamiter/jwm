# Extended X11 frame feedback

`JWM_X11_FRAME_SYNC=1` enables experimental extended X11 frame feedback in the
X11 compositor. It is off by default. It supports GTK clients advertising
`_NET_WM_SYNC_REQUEST` and the two-counter `_NET_WM_SYNC_REQUEST_COUNTER` property.
Patched Vulkan Ember/Frost clients also use these counters after separately
confirming producer publication. It does not change companion residency policy
or enable a compositor-owned body.

The protocol is described in the author's
[extended synchronization specification](https://fishsoup.net/misc/wm-spec-synchronization.html).
`_NET_WM_FRAME_DRAWN` records submission, not scanout. When precise presentation
timing is unavailable, the protocol permits a nearby actual buffer-swap time.
JWM uses actual `CLOCK_MONOTONIC` timestamps immediately before and after a
successful native swap. A measured zero offset remains zero (unknown); neither
an estimated timestamp nor a timer manufactures successful presentation.

The feedback reports scene inclusion of a client-ready counter; the client must
separately establish that its producer published the corresponding image. It is
**not** a companion `CompositorPresented` retirement certificate, GPU fence or
physical-display scanout certificate. The companion scene's stronger retirement
capability remains unchanged.

## Implementation

- A dedicated worker owns the X connection, XSync alarms, property checks and
  client messages. Rendering only loads immutable snapshots and sends bounded,
  nonblocking controls. Native events and an eventfd wake the worker; there is
  no recurring feedback timer when idle.
- Only observed, nonnegative, even counter values are candidates. Each window
  watch has a generation and a specific native counter resource. Replayed,
  future and previous-generation submissions cannot acknowledge the new watch.
- A candidate is captured before pixmap import. It is acknowledged only after
  its window participated in a successfully swapped scene with a current imported
  binding. Dirty, failed/deferred imports, fading windows, Genie animations and
  windows absent from the scene do not gain positive feedback.
- A pending ready frame in the scene requires composition; direct presentation
  cannot skip its required swap. Existing bypass policy applies after feedback.
- The worker checks actual compositor-selection ownership before feedback and
  validates the current counter resource. Destroy/unmap/property-change/alarm
  destruction invalidates a watch. Buffered X events are preserved before
  waiting on the socket, including events queued while reading replies.
- Root capability advertisement and cleanup are tied to the actual compositor
  owner, preserving unrelated `_NET_SUPPORTED` hints. A replacement worker's
  advertisement cannot be removed by an old owner. Queue overflow or connection
  failure retires this worker instead of silently accepting lost controls.

This is a local-session X11 path. Full odd-frame image freezing, remote-X clock
calibration, multi-monitor presentation timing and physical scanout validation
remain outside its verified scope. In particular, withholding odd-counter
feedback is not a claim that every other rendering path freezes an odd frame.

## Reproduce the native smoke

Build JWM with `--no-default-features --features backend-x11rb`. In a sibling
`jcompanion` checkout containing `tests/native_windows.py`, run:

```sh
env -u SSH_CONNECTION -u SSH_TTY XDG_SESSION_ID=eva-native-test \
  python3 tests/native_windows.py --daemon target/debug/jcompaniond \
  --host forge=/absolute/forge --host anvil=/absolute/anvil \
  --wm /absolute/jwm --frame-sync --freeze-owner --restart-daemon
```

This runs private Xvfb, DBus, home/XDG directories and executable snapshots. It
does not replace the user's window manager. It requires Xvfb, GLX software
rendering, dbus-daemon, xdotool, ImageMagick and Pillow. Repeat without
`--frame-sync`, and without the two fault flags, for the identical-binary
negative control.

The 2026-10-30 source build passed Forge↔Anvil, Ember↔Forge and Frost↔Forge
round trips, four-second stopped-owner blocking and coordinator SIGKILL/restart
recovery in this isolated X11/GLX environment. Ember used its real interactive
shell and a verified local printf task. The identical JWM binary without frame
feedback remained awaiting removal proof. Reports and representative PNGs are
in sibling `jcompanion/docs/evidence/native-jwm-*-2026-10-30/`.

Screenshots are samples, not continuous pixel-uniqueness evidence. Screenshot
capture precedes the journal read: a sample may show an earlier image beside a
later journal phase. The mint-ink detector only classifies the default Cat skin
under the tested colors, and zero matches do not certify absence. Screenshot
intervals and test duration are not transfer-latency measurements. Hardware,
Wayland, IME, audio, model API and performance acceptance remain separate.

## Eight-window continuation, 2026-10-31

The real eight-GUI / 32-PTY test exposed fixed-order starvation in the 3ms
ordinary-window texture refresh budget. Ordinary clients now rotate after the
last actually successful refresh; focused clients and bars retain priority.
Failed imports, audio pacing and existing budget limits are unchanged. The
one-client-per-frame service regression and the compositor scope pass 298
tests; strict all-target Clippy and XCB compilation pass.

The first full native matrix after that correction still failed. The former Forge freeze/recovery stall
advances after this correction, but a later compositor screenshot shows two
Ember bodies at once. A WGPU/native queue-present callback can precede removal
of the old window image from the composited result. This requires compositor-
correlated native clearance, not merely a scheduling improvement. The feature
and the earlier pairwise smokes alone were not a full multi-instance uniqueness gate.
See sibling `jcompanion/docs/evidence/matrix-checks-2026-10-31.json` and the
retained `023-waiting-ember-2.png` in the after-fairness report directory.

`frame_sync` debug records (watch resource, observed counter, publication,
successful submission and invalidation) run on its worker. JWM's logger sends
debug output to its per-process log file; the normal stdout copy includes Info
and higher levels. Inspect the correct process's file when tracing feedback.

## Producer publication and pacing continuation, 2026-10-31

Ember/Frost now require an exact Vulkan present ID to complete through
[`vkWaitForPresentKHR`](https://docs.vulkan.org/refpages/latest/refpages/source/vkWaitForPresentKHR.html)
before publishing an even counter. Queue submission, generic damage, resource
recycling fences and timeout are not substitutes. The clients retain the native
image until the matching drawn/timing pair completes, and bind the receipt to
the original UI pass/snapshot. Unknown timing releases pacing for a fresh retry
without proving presentation; resize and compositor-owner changes invalidate old
evidence. Only JWM's owner-bound `_JWM_FRAME_SYNC_OWNER` handshake enables this
prototype. Other backends/compositors remain unsupported.

An intermediate implementation without this pacing failed at Ember→Ember:
successive ready counters kept invalidating an earlier scene acknowledgement.
The coordinator remained awaiting removal proof, with no replacement granted.
That failure is retained in sibling
`jcompanion/docs/evidence/native-matrix-unpaced-publication-2026-10-31/`.

The paced implementation passed the full private 8-GUI / 32-PTY matrix: 72 real
input PID markers, eight native host identities/connections, four-second owner
freeze, all forward visits, 24 rapid focus/pane operations and full reverse
traversal. All 70 root-image samples detected at most one default-Cat body; 27
Vulkan publication records have matching accepted native scene receipts. The
same final binaries passed Ember↔Forge and Frost↔Forge freeze/restart round trips.
Forcing the unsupported software Vulkan driver preserved awaiting-removal state
and emitted no publication/retirement receipt. Its raw native report remains
failed, as expected; the negative expectation is recorded separately.

Reports, original PNGs, numeric feedback, binary hashes and harness snapshots
are in sibling `jcompanion/docs/evidence/frame-feedback-checks-2026-10-31.json`.
These results supersede the earlier failing reproduction only for this sampled
native configuration. Continuous/physical uniqueness, full odd-image freezing,
Wayland/other drivers, IME, voice/API and performance remain unaccepted. This does
not upgrade the companion compositor node to `CompositorPresented`.
