# Remote-control optimization implementation

Baseline: `c8cf77ec` (2026-11-06). These increments implement the
measurement, scheduling, resource-reuse and asynchronous presentation foundations
of the shared plan.
It is not completion of the entire remote-control roadmap.
This document retains historical implementation stages and their introduction
versions. For current usage and compatibility, see [remote-control.md](remote-control.md);
the current application protocol is v26, separate from transport v3.

## Implemented

- Capture-start to ACK latency now includes capture. Capture-complete mailbox
  age remains separate. Host `--stats-json` emits bounded-window quantiles and
  stage timing, including separate atlas packing/JPEG durations; see
  `remote-control.md` for exact semantics.
- Quality repair reuses a single pixel-damage scan even when ACK processing
  changes the quality epoch before encoding.
- Viewer network writes have a single owner outside the UI loop. Control and
  clipboard queues have count and byte limits. Key/button edges retain order,
  release fences discard stale input, and shutdown has reserved capacity.
  A bounded record deadline closes a stalled transport.
- Clipboard ingress and outbox are bounded. Local copies supersede pending old
  copies, compatible cumulative ACKs coalesce, overload fails explicitly, and
  shutdown cannot wait behind a full queue.
- CPU scaling caches geometry weights and scratch storage and no longer creates
  OS threads per frame. Large images use a persistent pool capped at four
  workers; small images and failed pool initialization use the serial path.
- Capture RGB allocations return through an ownership-based bounded recycler on
  completion, supersession, error and shutdown.
- The decoder publishes complete immutable tile snapshots, sharing unchanged
  tiles. The viewer transfers its RGB canvas allocation and patches only changed
  tile data, including changes from skipped mailbox snapshots. Keepalives share
  all tiles and do not copy pixels. Native-size display converts/uploads only
  the changed bounding rectangle. Scaled CPU presentation uploads conservative
  Triangle-filter-expanded damage using exact partial resampling when bounded
  scratch preflight succeeds; full-region or unsupported cases use full-image
  resampling. XRender patches its retained source and still composites the complete
  destination.
  At most eight merged rectangles bound request count; fragmented damage falls
  back to a bounding rectangle, and 60% covered area uses the complete image.
- SHM uploads use a bounded three-slot pool. Exact completion ownership prevents
  reuse while X11 consumes a segment; input polling continues while completion
  and server-copy fences are pending. Frame ACK follows both conditions, never
  receipt alone. Geometry setup and initial resource allocation may still block.
- At the historical v19 stage, peers negotiated an explicit supported stream
  profile and sent the initial viewport before capture. That stage required
  matching v19 peers and rejected prior versions without silent downgrade.
  Tile video and cursor state carry a geometry epoch. Explicit source resize was introduced in v15; the subsequent
  v16 keyboard, v17 pointer and v18 scroll capabilities are separately negotiated.
- Independent bounded cursor shape/position records are negotiated when host
  XFixes and viewer XRender formats support them. Cursor movement avoids video
  readback/encoding and uses a cached ARGB overlay without per-motion GetImage.
  Unsupported endpoints retain composited video. Outside-region cursor samples
  use a transparent sentinel and normalized hidden position; fresh sprite
  admission uses its own sampled coordinates. Capture stalls can still delay
  cursor polling; this is not a dedicated capture connection/thread.
- Clipboard file limits use the minimum of both peers, with zero disabling files;
  a host cannot enlarge the viewer request. Decoded-canvas width, height and pixel
  budgets are also selected as the peer minimum and enforced before capture and
  canvas allocation, including later resize. Padded atlas/scratch and source
  readback have separate bounds; this is not a total-memory limit.
- Saved profiles support explicit init/save/list/show/run/remove, required
  permission booleans, bounded private storage, atomic replacement and locking.
  They store supported connection/SSH quality/clipboard settings, never key
  contents or arbitrary SSH commands. Running a profile is always explicit.
  Explicit `--window-size WIDTHxHEIGHT` can be saved and reapplied locally, with
  bounded dimensions/pixels and unchanged defaults. It is an initial client-size
  request, not observed-state restoration; the window manager remains authoritative.
  Explicit `--window-position X,Y` saves a bounded local best-effort position.
  Current-monitor visibility checks reject off-screen placement with a warning
  and fallback; accepted positions use a WM hint, never forced placement.

- Protocol v11 adds immutable host-authored source grants (maximum 32) and
  local viewer cycling with Ctrl+Super+Shift+the configured escape key, also
  in view-only mode. A scoped initial selection never implicitly grants root.
  Source changes release input and reset frame/cursor state; unsafe transitions
  fail closed. Extra grants are explicit on host/SSH CLI and cannot be saved in
  profiles yet. The v19 stage required a coordinated upgrade of both peers;
  it is not the current compatibility version.
- This source-switching work is covered by offline tests only in this checkout;
  no new live-X11, hardware timing, or benchmark result is claimed here.

- Protocol v12 negotiates immutable clipboard content and direction limits.
  Both peers must consent; explicit CLI/SSH controls and saved profile narrowing
  preserve legacy all/both defaults without granting clipboard permission.
  Capture policy is enforced before requesting local payloads, and ingress and
  transfer workers independently reject unnegotiated content. One-way transfers
  retain reverse-direction ACK/refusal control traffic. Live-X validation remains
  pending; offline policy tests do not establish clipboard timing or desktop behavior.
- The remote X11 clipboard boundary now has a one-value latest-capture mailbox
  and an ordered serve queue capped at 32 records / 33 MiB retained payload
  allocation. Image capacity is capped at 32 MiB, text at 256 KiB; file lists
  have at most 4096 entries and 256 KiB retained path allocation, with a
  conservative 256 KiB preflight bound for all three encoded offers. Queue
  admission never waits for space. Each normal pass serves at most eight
  requests before X events and expiry processing. Overload uses a reserved
  sticky abort path, refuses pending pastes, releases only this watcher's
  ownership, closes captures, and propagates failure to the remote session.
- Transfer-worker ingress and outbox accounting charges retained capacities.
  Rejected or panicking target calls close the session; an independent pending
  token permits refusal even while fulfil consumes the incoming sink. Received
  files are paired with their staging receipt before submission, so ambiguous
  callback outcomes remain retained. Only proven unpublished rejection reclaims
  a completed transfer. The status
  says `clipboard update queued`: admission is not proof that X11 applied it.
  Per-transfer receipts reclaim only files explicitly rejected before entering
  a clipboard offer. Backend failure alone does not prove previously advertised
  paths are unused, so accepted or uncertain copies retain the existing bounded
  newest-directory policy rather than being eagerly deleted.
  Retention alone is not a global staging-disk bound: the sweep skips live PIDs,
  so multiple sessions in one process can retain multiple completed directories.
  The optional local admission cap described below separately accounts logical
  archive reservations across cooperating processes sharing one cache root.
  This bounds the application queues, not total process/X-server memory or
  synchronous X11/disk call duration. Generic native clipboard history retains
  its existing channel semantics. Live overload/paste tests remain ignored here
  and are configured for isolated-Xvfb CI, not claimed as locally verified.

## Explicitly deferred

- Observed session-state restoration, exact window-manager placement, XKB/IME,
  runtime clipboard expansion/direction changes, partial host readback, Wayland and hardware
  encoding. Explicit saved viewer size and best-effort requested position are
  implemented; they do not observe or automatically restore window-manager state.
- Broader negotiated capabilities, mutable source authorization and backward-compatible extension framing.
- Input-to-visible instrumentation and live desktop workload replay.
  The offline synthetic tile-codec baseline is documented in
  [remote-replay-benchmark.md](remote-replay-benchmark.md); it measures neither
  capture/presentation nor end-to-end latency.

## Verification commands

```sh
cargo fmt --all -- --check
cargo check --locked --no-default-features --features remote-x11 --all-targets
scripts/test.sh --no-default-features --features remote-x11 --lib remote::
scripts/test.sh --no-default-features --features remote-x11 --lib backend::clipboard_x11::tests::
scripts/test.sh --no-default-features --features remote-x11 --bin jwm-remote
cargo clippy --locked --no-default-features --features remote-x11 --lib --bin jwm-remote --no-deps -- -D warnings
```

Native linking needs installed XCB and xkbcommon libraries. Full default-backend
validation additionally needs the development packages listed in CONTRIBUTING.
Unit tests do not establish real X11 presentation timing or hardware correctness.
Before deployment, compare identical idle/input/scroll/resize/file-transfer
workloads with and without these changes, including constrained bandwidth,
connection loss, Intel/AMD/NVIDIA, and a two-hour resource soak. Do not infer
latency or throughput improvements solely from passing unit tests.

## CPU-only measurement

On the same AMD EPYC 9V74 cloud container, deterministic release-mode scaling
used ten warmups and forty alternating-order samples. Median serial versus
four-worker pooled times were 7.77/3.57 ms (1080p to 720p), 16.25/7.17 ms
(1440p to 1080p) and 24.08/9.63 ms (4K to 1080p). 640x360 selected serial.
These measurements isolate CPU scaling; they are not end-to-end remote latency.

## Live X11 validation limit

An ignored isolated-X11 position test checks requested client coordinates/size
and accepted-only user-position hints, plus off-screen fallback with no hint.
It requires `JWM_TEST_X11_DISPLAY` without a window manager and has not run here;
even a pass would establish X-server requests, not WM placement compliance.

A task-local official Debian Xvfb and its dependencies were prepared, but this
runtime rejects creation of AF_UNIX sockets with EPERM, including a narrowly
scoped elevated startup attempt. The ignored live SHM test is provided for a
machine that can start an isolated X server. It has not run in this environment.

Reproduce the CPU comparison (release build, no live desktop required):

```sh
scripts/test.sh --release --no-default-features --features remote-x11 --lib remote::downscale::tests::benchmark_cached_serial_and_pool -- --ignored --nocapture
```

Run the live asynchronous presentation regression only on an isolated X server:

```sh
JWM_TEST_X11_DISPLAY=:177 scripts/test.sh --no-default-features --features remote-x11 --lib remote::x11_viewer::tests:: -- --ignored --nocapture --test-threads=1
```

## Reproducible transport faults and CI

`scripts/remote-fault-proxy.py` provides bounded, loopback-default directional
bandwidth/latency and stopped-reader/disconnection fixtures without kernel
network changes. See `remote-fault-proxy.md`. Its eight deterministic tests run
with `python3 scripts/test-remote-fault-proxy.py` and are included in CI.
The remote-only feature profile now runs the remote unit/CLI suite and isolated
Xvfb SHM/cursor/clipboard regressions. That CI job has been configured, not executed or verified from
this cloud workspace; live-X11 results remain pending until it runs.

## Compatibility note for Rust callers

The historical v19 wire stage was intentionally incompatible with prior
application protocol versions. Current application v26 likewise requires
matching peers. The Rust API notes below describe those historical increments,
not a complete current migration checklist.
The public `DecodedFrame` and synchronous `Viewer::draw` behavior remain usable.
`HostOptions`, `ClientOptions` and `SshOptions` now require `frame_limits`;
`StreamCapabilities` also adds that field. `FrameLimits::default()` retains the
previous hard ceilings for callers updating struct-literal construction.
`CapturedFrame` now includes a public `geometry_epoch` field, so external Rust
struct-literal constructors must initialize it to the source's nonzero epoch.
External Rust users constructing `ClientOptions`, `SshOptions`, or profile
`Viewer` literals must now supply `window_size: None` and
`window_position: None` to retain the defaults.
The public `x11_viewer::Viewer::connect` signature is unchanged.

External Rust callers of clipboard staging must now supply the full announced
archive stream length to `Staging::begin(archive_bytes)`. `ArchiveReceiver::new`
now returns `io::Result<Self>` because it pins and validates the private staging
directory before accepting bytes. These are Rust API changes, not wire changes.

The source identity must change when the selected geometry changes; dimensions
alone are not sufficient to authorize a different shared source.

## Benchmark context metadata

`scripts/remote-benchmark-metadata.py` emits bounded JSON context without network
requests or desktop access. GPU PCI identifiers are opt-in; display dimensions,
backend and build labels must be supplied explicitly. Missing values are null.
It does not collect hostnames, users, addresses or display content, and it does
not measure performance. See `remote-benchmark-metadata.md` for the exact fields.

## Explicit reconnect safety

Receiver failures retain their typed cause, and authenticated host Close remains
separate from transport loss. Cleanup errors preserve the foreground cause rather
than hiding sender/receiver failure. Explicit sender and receiver joins use a
bounded wait on one reaper per worker, so a stalled thread-local destructor does
not block the viewer's join path. Timeout reports incomplete cleanup and may leave
a detached worker/reaper; it is not permission to start a replacement session.

Clipboard sync, capture-forwarder and X11 worker shutdown share one explicit
deadline and preserve incomplete cleanup errors. Their Drop paths signal only;
stalled owned work may continue after a reported timeout, so timeout never proves
all workers exited. Host input and clipboard post-stream joins now share a
two-second budget; synchronous capture and outer joins remain outside it.
Actual window-close intent is latched before fallible local input release and
network flushes, and remains typed when those operations fail.

Explicit direct-connect retry is bounded to at most five additional sessions
with a lifetime budget and 1/2/4/8/8-second delays; default remains disabled.
Only tagged established receiver EOF/reset/abort errors with certified successful
local cleanup qualify. User/peer closes, local escape, viewer-observed
protocol/authentication failures, timeouts and any incomplete local cleanup do
not qualify. A bare EOF/reset cannot prove that an unreported host operational
failure did not cause it; remote terminal reporting remains best-effort. Pending local intent is
checked again after cleanup before certification. SSH and saved-profile retries
remain unsupported; SSH exit 255 cannot reliably distinguish network loss from
authentication or host verification failure. No session/transfer state resumes. The escape shortcut only releases
input grab; it does not close the session and is not treated as cancellation.

Clipboard policy adds `clipboard_mode` and `clipboard_direction` to
`HostOptions`, `ClientOptions`, `SshOptions` and saved-profile `Viewer` literals.
Use `ClipboardMode::All` and `ClipboardDirection::Both` for the previous scope;
the existing consent boolean still controls whether sharing is enabled at all.
`StreamCapabilities` adds explicit `clipboard_content` and
`clipboard_directions` masks, which must agree with hello flags and file limits.
`SyncOptions` adds local `send`, `receive` and `images` gates; callers must derive
these from negotiated permissions rather than treating them as new defaults.

## Partial CPU Triangle resampling

Scaled incremental CPU updates now sample only their dirty destination rectangles,
using the full destination grid. The private RGB sampler follows image 0.25.10's
Triangle arithmetic: full-source edge normalization, vertical then horizontal f32
accumulation, and only final u8 rounding. This is not a resize of a cropped source.
The compatibility reference is `image/src/imageops/sample.rs` from
[image 0.25.10](https://github.com/image-rs/image/tree/v0.25.10);
its MIT license is retained in `licenses/image-MIT.txt`.

The viewer preflights every rectangle and fallibly reserves reusable scratch before
uploading any patch. Scratch requests are capped at 16 MiB, including RGB output,
one intermediate float row and weights. Full-frame damage or an unsuitable plan
retains the existing single full resize. Source/XRender patches and identity-size
updates keep their previous paths. No transport, presentation, input, ownership,
or ACK fence changes are involved.

Pure CPU tests compare patch bytes directly against the dependency's full resize,
including exhaustive small dimensions and rectangles, random RGB fixtures, edge
and one-axis cases, rounding-sensitive patterns, reconstruction, invalid bounds,
budget rejection, and buffer reuse. These tests do not establish live-X11 latency,
frame rate, memory bandwidth, or end-to-end performance improvements.

A bounded CPU-only smoke comparison used Rust `-O`, the release image 0.25.10
library, deterministic RGB pixels `[(71*x+23*y) mod 256,
(13*x+97*y) mod 256, (31*x+11*y) mod 256]`, and 40 iterations per case, alternating full/partial timing order after one
warmup of each. The reproducible harness is `scripts/remote-triangle-microbench.rs`.
Partial timings include each iteration's preflight and all patch renders; full
resize timings include allocation. Each patch was checked byte-for-byte first.
Tiny damage was a central 8×8 destination patch; scattered damage was four 8×8
corner patches; near-full damage omitted only the last destination column.
One local, non-isolated run recorded these aggregate microseconds (full / partial):

- 320×180 → 640×360: tiny 187308 / 68; scattered 174956 / 190;
  near-full 183417 / 138658. Full-region fallback: 204422.
- 640×360 → 320×180: tiny 115587 / 138; scattered 108491 / 474;
  near-full 74146 / 57048. Full-region fallback: 92375.

The near-full case exercises the raw sampler, not the production viewer, whose
`DamageRegions` collapses coverage of at least 60% to the full-region fallback.
The explicit fallback case times rejected preflight plus full resize.

These are smoke measurements, not a statistically controlled benchmark. They
exclude X11 conversion/upload, encoding, transport, presentation, and interactive
latency. They do not justify a general speedup or desktop performance claim.

### Viewer allocation ceilings

The viewer validates startup geometry and local X11 resize notifications before
changing its viewport or requesting replacement pixmaps. Direct decoded-frame
callers and reusable native images also enforce the existing hard limits: 16384
pixels per axis and 64 MiPixels total. These limits preserve normal 8K fullscreen;
the smaller explicit startup-size budget remains unchanged. Transient zero-sized
ConfigureNotify dimensions retain their previous normalization to one pixel,
while zero-sized direct inputs are rejected. Oversized events fail explicitly
rather than silently clamping the viewport and changing input coordinates.

This bounds resources requested in response to trusted local X11 events; it is
not a remote-exploit claim or a low-memory guarantee. Full Triangle resizing,
retained frames, native images, server pixmaps and in-flight SHM buffers can still
consume substantial memory within the hard cap. Partial Triangle scratch keeps
its separate 16 MiB cap. Pure tests cover boundaries and guard ordering without
large allocations. Live-X11 behavior and memory/latency measurements remain
unverified for this guard.

### Bounded post-stream host cleanup

After frame streaming returns, input and clipboard workers share a two-second
cleanup budget. The input reaper confirms a completed thread join, including
thread-local destructors, rather than merely observing the worker body return.
Clipboard shutdown is still attempted with the remaining budget, including zero
when input cleanup consumed it. An earlier stream failure remains visible when
cleanup also fails.

Unconfirmed cleanup or a session/input panic permanently closes that listener's
session admission before releasing the viewer slot. Restart the host to admit a
new controller. This prevents a detached old input worker from overlapping a new
controller; a timeout does not mean held input was released. Ordinary session or
protocol failures with confirmed cleanup do not poison admission. A one-session
host reports incomplete cleanup as failure. Requested clipboard startup failure
also fails the session and poisons admission instead of silently continuing
without clipboard: the current startup error cannot prove that no background
worker started, even when the reported failure is simple backend unavailability.

This deadline applies only after streaming returns. Synchronous capture,
frame-sender shutdown and outer connection-thread joins can still block, so it
is not a bounded whole-host termination guarantee. The regression tests use
synthetic workers and do not establish live X11 release behavior.

### Content-free command diagnostics

The global opt-in `--diagnostics-json` flag emits one best-effort JSON line on
stderr after a successfully parsed command returns. Its type is
`jwm_remote_command_result` and schema version is 2. Fixed-vocabulary fields
identify the command, success/error outcome, known client stage, error category,
I/O kind, cleanup status and existing reconnect certificate. Nested profile runs,
reconnect attempts and SSH helpers do not emit additional records. The flag is
not forwarded to the remote host or stored in profiles.

The JSON contains no formatted error, peer-supplied close reason, endpoint,
profile name, key, clipboard contents or input data. Existing human stderr is
unchanged and may contain contextual details; this privacy guarantee applies
only to the JSON record. Consumers should select records by type, not assume
stderr is a JSON-only stream.

Cleanup is `unknown` unless typed evidence establishes `complete` or
`incomplete`; even successful commands can report unknown, since SSH may only
focus an existing viewer. `reconnect_certified` describes the existing direct
client retry certificate, not remaining retry budget or permission for an
external program to retry. Cleanup failure always clears it. A write failure
never changes the command's result. Parse errors, help/version exits, signals,
process aborts and hung commands do not promise a terminal record. This adds no
wire protocol or capability changes.

### Unpublished file-transfer receipts

Each incoming file fulfilment carries one small atomic disposition receipt tied
to its exact owned staging directory. A stale token that never enters a pending
or current offer, or a command rejected/removed before backend application, can
be marked explicitly unpublished. The transfer worker reclaims only that
directory; the X11 event loop performs no filesystem deletion. A reused numeric
token or another session cannot redirect the receipt to a newer directory.

Acceptance is marked before a deferred paste can publish paths. Accepted,
dropped, panicked or otherwise uncertain outcomes conservatively retain files
under the existing newest-three active / newest-one shutdown retention policy.
Backend failure or clipboard ownership loss is not proof that a file manager
or clipboard manager has finished using advertised paths. A receipt resolved
after the transfer worker exits does not trigger a new cleanup worker.

This improves reclamation of proven unpublished transfers, not global disk
bounding or indefinite paste retention. The existing PID-based sweep and retention
policy still apply. Pure queue, staging and mock-target tests establish receipt
ownership transitions; real clipboard-manager handoff and ownership races
remain unverified without an isolated X11 session.

### Opt-in local clipboard staging admission

`jwm-remote clipboard-staging status|set-limit SIZE|disable` manages one local
private cache root. No numeric cap is selected by default. `set-limit` is the
explicit activation step; status does not create, delete, reconcile or repair
persistent state, and disable preserves accounting and staged files. A lower
limit than current accounted usage is rejected without changing the cap.
The setting never enters SSH arguments or saved profiles, and adds no wire
field or protocol-version change. See [activation and size syntax](remote-control.md#optional-local-clipboard-staging-cap).

Upgraded uncapped sessions already journal exact reservations. Before first
activation, the versioned `legacy_pending` state permits coexistence with old
live writers without sweeping them. Read-only status and activation refuse an
incomplete total if any untracked writer is live. Safe inactive legacy copies
are inventoried; first successful activation imports them and permanently
requires tracked writers. Disabling changes only the byte cap and never restores
the legacy-coexistence phase. Missing parts of initialized metadata, corrupt
state and mismatched directory identities fail closed rather than resetting
usage. Old binaries remain outside this advisory protocol and must not be used
with the root after activation.

Full incoming logical archive bytes are reserved before admission, and active
and retained copies share that accounting across cooperating processes. No
copy is evicted to satisfy the cap. Proven unpublished receipt reclamation and
the existing newest-three active / newest-one shutdown policy remain separate
lifecycle decisions. Cleanup must establish removal before releasing the
corresponding reservation; uncertain cleanup remains charged rather than
making room on speculation.

The bound is not total disk usage, allocated blocks, metadata/inodes, or a quota
against old binaries and same-user writers. Separate cache roots have separate
budgets. Root-lock contention is an immediate refusal; bounded metadata and
inventory do not make synchronous filesystem I/O time-bounded. Limits include
4 MiB serialized metadata, 1,024 recorded sessions, 16,384 reservations, 1,088
root entries and payload-tree walks of at most 1,000,000 entries, depth 64 and
4,096 relative-path bytes. These accounting limits also apply with no byte cap.
Inventory or state-validation failures do not silently reset accounting or enable a cap.
The per-copy file limit, free-disk check and clipboard consent remain unchanged.

The CLI uses exact checked sizes rather than floating-point conversion, and
reports exact byte counts. Content-free diagnostics add fixed command names
`clipboard_staging_status`, `clipboard_staging_set_limit` and
`clipboard_staging_disable` under existing schema 2; they do not include cache
paths, sizes or inventory contents. CLI and accounting tests are offline;
live paste/ownership behavior, physical disk pressure and a long-running
multi-process soak remain separate validation requirements.

### Structured authenticated peer termination

Application protocol v13 adds a bounded host-only SessionClose record with five
existing operational causes: busy, admission disabled, capture unavailable,
clipboard unavailable and host stopping. Its strict UTF-8 codec caps the human
reason at 240 bytes and rejects unknown tags, malformed lengths and trailing
data. This record originally used encrypted transport v2; current endpoints
require transport v3 independently. Both application endpoints must upgrade together. This is a concrete record, not a generic extension
framework or backward-compatible version negotiation.

The viewer preserves the typed code through negotiation, initial-frame wait
and established reception. Old or invalid Hello replies retain legacy text
Close, and legacy reason text never impersonates a structured code. Command
diagnostic schema 2 adds nullable `peer_close_code` with a fixed snake_case
value; raw reasons remain excluded. Host-stats telemetry keeps schema 1.

Every peer close is terminal, including admission and startup failures. Remote
codes do not authorize reconnection or assert local cleanup status. A local
cleanup failure retains diagnostic priority and clears reconnect certification,
while preserving the peer code separately. Best-effort host goodbye still uses
its existing nonblocking lock attempt and bounded write deadline. These changes
are covered by offline authenticated-record and codec tests, not live graphical
or performance acceptance.

### Best-effort runtime capture and clipboard failure reporting

The host reuses v13 `CaptureUnavailable` and `ClipboardUnavailable` records for
identified runtime producer failures. Capture classification is attached only
to source switching, cursor sampling, activity waits and frame capture calls;
other failures in the capture loop do not acquire that code. A local clipboard
outbox loss or confirmed worker-start failure is distinguished from socket
cloning and transport write errors. Human reasons sent for these failures are
fixed strings; the original contextual error remains local.

Shutdown still marks the session stopped and closes its frame mailbox/credits
first. Read-side socket shutdown wakes the input reader before one bounded
outgoing notification attempt, then full socket shutdown follows. The notifier
uses a nonblocking writer-lock attempt and the existing 250ms record deadline.
It never waits for ownership, retries a notification, or appends to a poisoned
partial-record stream. Concurrent termination keeps the original first cause;
notification failure does not replace the primary error.

This is best-effort reporting, not guaranteed delivery or whole-host bounded
termination. Another shutdown, writer contention, poison or deadline expiry can
leave the viewer with an ambiguous EOF/reset. Input/keymap and local encoder
failures are not relabeled as capture or clipboard failures. Receiving a valid
terminal record suppresses automatic reconnect; failure to receive one cannot
prove the host had no operational error. Wire v13 and diagnostic schema 2 are
unchanged. Offline authenticated-record and shutdown-order tests do not replace
live X11 held-input and failure acceptance testing.

### Terminal reporting for local video preparation failures

Local tile planning, refinement and JPEG-encoding errors are distinguished
from frame-credit timeouts, source coordination and socket operations. When
such a local error owns the session's first stop cause, the video worker may
send a fixed legacy Close reason through the same bounded notification path.
The detailed error stays local. It is not mislabeled as capture unavailable or
host stopping, and no new application version or structured code is introduced.

The worker publishes stopped state and closes its waiters before read-side
shutdown, then makes at most one nonblocking-lock/250ms notification attempt
before full shutdown. All encoding and writer guards have unwound first. A
failed notification cannot replace the original error, revive a poisoned
record stream or commit the encoder's sent-frame baseline. Contention and
concurrent teardown can still prevent delivery.

An authenticated legacy Close remains terminal in both first-frame and steady
reception. Command diagnostics report `peer_closed` with `peer_close_code: null`;
the fixed human reason is not included in JSON and never grants reconnect
permission. Local cleanup failure keeps priority. The per-instance encoder
fault fixtures used for regression tests exist only under `cfg(test)` and add
no production setting or runtime fault behavior. Input/keymap reporting uses
the distinct post-release lifecycle described below; the existing ambiguous-EOF
caveat still applies.

### Post-release input and keymap failure reporting

Selected local X11 input failures and confirmed keymap failures may produce a
fixed legacy Close, without changing v13 or diagnostic schema 2. Classification
is attached to actual X11 connection/reply failures, local keymap reads and
confirmed incompatible mappings. Invalid peer input, decoding, network reads,
source authorization, ACK validation, clipboard ingress and missing-keymap
invariants are not relabeled as backend failures.

The winning input stop cause is captured before publishing stopped state and
closing frame credits. Optional notification is derived from the original
classified error, not from later cleanup wrappers or the current atomic cause.
Explicit input release and injector destruction finish before the outer input
worker can attempt any notification. A blocked release or destructor therefore
cannot postpone stopped-state visibility, and no notice is sent ahead of a
pending destructor's release retry. Panics retain the existing join/admission
failure handling rather than emitting a success-like notification.

Cleanup errors remain intact even if a destructor later completes. Final release
failure after an otherwise normal close does not create a new notice. The
thread's result type, cleanup reaper and admission poisoning remain unchanged.
The notification makes no claim that thread-local destructors or other workers
have exited, or that all held input was successfully released. Other teardown
paths may close the transport first; no new coordination wait guarantees
notification delivery. Received legacy Close is terminal with a null peer code;
local cleanup failure retains diagnostic priority. Offline fake-injector
release/destructor tests do not replace real X11 input-release acceptance.


### Runtime clipboard content narrowing

Application protocol v14 adds bounded, versioned restriction request/acknowledgement
records. The viewer's Ctrl+Super+Alt+configured escape key narrows the initial
content grant through all, text + images, text and off, including in view-only
mode. It consumes shortcut edges and releases remotely held modifiers. Initial
directions remain fixed; there is no re-enable, profile mutation, GUI or IPC
control. An originally clipboard-off session remains inactive.

The local gate changes immediately. A pending title/terminal status is distinct
from confirmation: both endpoints must cross transfer-worker and backend fences,
and the host acknowledgement must pass its ordered outgoing stream. A reader
merely parsing a request is not a completed restriction. One request may be
pending; invalid masks, expansion, stale revisions and unsolicited or duplicate
acknowledgements fail closed. Timeout ends the session without restoring sharing.
Reserved control capacity prevents ordinary clipboard queue saturation from
blocking restriction admission.

Capture, transfer processing, inbound acceptance, backend fulfilment and final
outgoing dispatch enforce the narrowed content grant. Forbidden active transfers
are cancelled and unfulfilled offers abandoned. Discarded outbound data retains
cumulative flow accounting through bounded content-free skip records, avoiding
resets that could lose credit or strand later permitted transfers. File receipts
release only proven-unpublished staging; advertised paths retain the existing
retention policy.

Previously transmitted bytes, clipboard history, exposed files and pastes already
in progress cannot be erased by this control. Synchronous native work can delay
confirmation. Deterministic state/queue tests are separate from live shortcut,
clipboard-owner and paste acceptance, which remains unverified in this checkout.

## Experimental v19 scroll-copy opt-in

Host `--scroll-copy` is default-off and does not grant scroll input. It adds a
negotiated software codec path for exact full-width vertical shifts of ±1..64
encoded-image pixels, with at least half-frame overlap. Source, geometry and base-sequence
identity are required; other motion, static headers/toolbars, unrelated changes
and unsupported resize transitions retain ordinary tile encoding. The previous
decoded overlap is copied without another JPEG quantization pass, but image
error is not guaranteed to improve. This is not GPU encoding or a latency claim.
Same-input, same-quality offline A/B measurements are required before considering
a broader default; old application-protocol peers reject v19 negotiation.

### Bounded synthetic A/B evidence

One debug-build pass ran each arm once over the same six deterministic scenes,
eight offered frames per scene, at JPEG quality 75. The normal size is 321×193;
the odd-resize case changes to 303×181 after frame four. Each input frame's
SHA-256 is asserted identical between the default-off and explicitly enabled
arms. Payload totals include the initial keyframe. MSE is total squared channel
error divided by total channel samples across all eight frames, including
suppressed frames; it is not an average of rounded per-frame values.

| Synthetic scene | Default-off payload bytes | Scroll-copy payload bytes | Default-off MSE | Scroll-copy MSE |
| --- | ---: | ---: | ---: | ---: |
| Static | 56,004 | 56,004 | 230.97731 | 230.97731 |
| Up 7 pixels/frame | 448,827 | 106,253 | 231.30315 | 230.95062 |
| Down 7 pixels/frame | 448,418 | 92,378 | 231.37526 | 233.60609 |
| Random changes | 449,610 | 449,610 | 231.03998 | 231.03998 |
| Fixed toolbar with scrolling content | 417,077 | 417,077 | 232.17857 | 232.17857 |
| Odd-size resize | 104,704 | 104,704 | 230.85955 | 230.85955 |

Both scroll scenes exercised production copy records. The other four scenes
emitted no copy records, and their byte totals and unrounded SSE/sample ratios
were identical between arms. Downward scrolling increased MSE in this pass:
copying already decoded pixels does not guarantee better quality than a fresh
JPEG update. These inputs are synthetic, not a desktop recording, physical
DRM presentation or two-machine test. No CPU speedup or end-to-end latency
improvement is inferred from this one pass. Host `--scroll-copy` remains off
by default.

The benchmark was rerun after JPEG-envelope validation hardening. All 96
per-frame payload sizes, squared errors and input hashes matched the initial
valid-JPEG run exactly. The boundary checks reject incomplete/trailing JPEG
envelopes; they are not a proof of every entropy-coded coefficient's validity.

Equivalent reproducible command (the initial pass ran the already-built debug
test executable with this exact test filter):

```sh
cargo test --locked --no-default-features --features remote-x11 --lib \
  remote::tile::scroll_benchmark::tile_scroll_copy_offline_ab_benchmark \
  -- --ignored --exact --nocapture
```

The ignored test emits per-frame and aggregate JSON. It opens no display or
socket and uses no GPU. Both arms together are bounded to 64 MiB of offered
source RGB; inputs are generated one frame at a time, not retained as a replay
history. Generation, input hashing and distortion calculation are excluded
from the reported codec-stage timings.
