# Remote-control optimization implementation

Baseline: `c8cf77ec` (2026-11-06). These increments implement the
measurement, scheduling, resource-reuse and asynchronous presentation foundations
of the shared plan.
It is not completion of the entire remote-control roadmap.

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
  Triangle-filter-expanded damage while retaining full-image resampling; XRender
  patches its retained source and still composites the complete destination.
  At most eight merged rectangles bound request count; fragmented damage falls
  back to a bounding rectangle, and 60% covered area uses the complete image.
- SHM uploads use a bounded three-slot pool. Exact completion ownership prevents
  reuse while X11 consumes a segment; input polling continues while completion
  and server-copy fences are pending. Frame ACK follows both conditions, never
  receipt alone. Geometry setup and initial resource allocation may still block.
- Protocol v12 negotiates an explicit supported stream profile and sends initial
  viewport before capture. v6/v7/v8/v9/v10/v11/v12 peers must update together; there is no silent
  downgrade. Tile video and cursor state carry a geometry epoch.
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
  profiles yet. Both peers require a coordinated v12 upgrade.
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
  files become completed staging only after serve-queue admission. The status
  says `clipboard update queued`: admission is not proof that X11 applied it.
  Backend failure observed before transfer-worker cleanup also removes completed
  staging. A later backend failure after that worker exits can leave the bounded
  newest completed directory per session under the existing sweep policy.
  This is not a global staging-disk bound: the sweep skips live PIDs, so multiple
  sessions in one process can retain multiple completed directories.
  This bounds the application queues, not total process/X-server memory or
  synchronous X11/disk call duration. Generic native clipboard history retains
  its existing channel semantics. Live overload/paste tests remain ignored here
  and are configured for isolated-Xvfb CI, not claimed as locally verified.

## Explicitly deferred

- Partial CPU resampling, observed session state
  restore, exact window-manager placement, XKB/IME, runtime clipboard
  mode changes, local readback, Wayland and hardware encoding.
- Broader negotiated capabilities, mutable source authorization, structured errors and backward-compatible extension framing.
- Input-to-visible instrumentation and live desktop workload replay.
  The offline synthetic tile-codec baseline is documented in
  [remote-replay-benchmark.md](remote-replay-benchmark.md); it measures neither
  capture/presentation nor end-to-end latency.

## Verification commands

```sh
cargo fmt --all -- --check
cargo check --locked --no-default-features --features remote-x11 --all-targets
cargo test --locked --no-default-features --features remote-x11 --lib remote::
cargo test --locked --no-default-features --features remote-x11 --lib backend::clipboard_x11::tests::
cargo test --locked --no-default-features --features remote-x11 --bin jwm-remote
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
cargo test --release --locked --no-default-features --features remote-x11 --lib remote::downscale::tests::benchmark_cached_serial_and_pool -- --ignored --nocapture
```

Run the live asynchronous presentation regression only on an isolated X server:

```sh
JWM_TEST_X11_DISPLAY=:177 cargo test --locked --no-default-features --features remote-x11 --lib remote::x11_viewer::tests:: -- --ignored --nocapture --test-threads=1
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

Wire v12 is intentionally incompatible with prior application protocol versions.
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
all workers exited. The existing host input-thread join is still unbounded.
Actual window-close intent is latched before fallible local input release and
network flushes, and remains typed when those operations fail.

Explicit direct-connect retry is bounded to at most five additional sessions
with a lifetime budget and 1/2/4/8/8-second delays; default remains disabled.
Only tagged established receiver EOF/reset/abort errors with certified successful
cleanup qualify. User/peer closes, local escape, protocol/authentication failures,
timeouts and any incomplete cleanup do not qualify. Pending local intent is
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
