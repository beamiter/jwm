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
- Protocol v9 negotiates an explicit supported stream profile and sends initial
  viewport before capture. v6/v7/v8/v9 peers must update together; there is no silent
  downgrade. Tile video and cursor state carry a geometry epoch.
- Independent bounded cursor shape/position records are negotiated when host
  XFixes and viewer XRender formats support them. Cursor movement avoids video
  readback/encoding and uses a cached ARGB overlay without per-motion GetImage.
  Unsupported endpoints retain composited video. Outside-region cursor samples
  use a transparent sentinel and normalized hidden position; fresh sprite
  admission uses its own sampled coordinates. Capture stalls can still delay
  cursor polling; this is not a dedicated capture connection/thread.
- Clipboard file limits use the minimum of both peers, with zero disabling files;
  a host cannot enlarge the viewer request. This does not negotiate variable
  frame pixel budgets or change existing fixed safety bounds.
- Saved profiles support explicit init/save/list/show/run/remove, required
  permission booleans, bounded private storage, atomic replacement and locking.
  They store supported connection/SSH quality/clipboard settings, never key
  contents or arbitrary SSH commands. Running a profile is always explicit.

## Explicitly deferred

- Partial CPU resampling, session
  restore/source switching, window-geometry restoration, XKB/IME and clipboard
  UX modes, local readback, Wayland and hardware encoding.
- Variable decode pixel/dimension budgets and broader negotiated capabilities,
  session epochs and dynamic source
  authorization, structured errors and backward-compatible extension framing.
- Input-to-visible instrumentation and a fixed workload replay harness.

## Verification commands

```sh
cargo fmt --all -- --check
cargo check --locked --no-default-features --features remote-x11 --all-targets
cargo test --locked --no-default-features --features remote-x11 --lib remote::
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
Xvfb SHM/cursor regressions. That CI job has been configured, not executed or verified from
this cloud workspace; live-X11 results remain pending until it runs.

## Compatibility note for Rust callers

Wire v9 is intentionally incompatible with prior application protocol versions.
The public `DecodedFrame` and synchronous `Viewer::draw` behavior remain usable.
`CapturedFrame` now includes a public `geometry_epoch` field, so external Rust
struct-literal constructors must initialize it to the source's nonzero epoch.
The source identity must change when the selected geometry changes; dimensions
alone are not sufficient to authorize a different shared source.

## Benchmark context metadata

`scripts/remote-benchmark-metadata.py` emits bounded JSON context without network
requests or desktop access. GPU PCI identifiers are opt-in; display dimensions,
backend and build labels must be supplied explicitly. Missing values are null.
It does not collect hostnames, users, addresses or display content, and it does
not measure performance. See `remote-benchmark-metadata.md` for the exact fields.

## Reconnect prerequisites, not automatic reconnect

Receiver failures retain their typed cause, and authenticated host Close remains
separate from transport loss. Cleanup errors preserve the foreground cause rather
than hiding sender/receiver failure. Explicit sender and receiver joins use a
bounded wait on one reaper per worker, so a stalled thread-local destructor does
not block the viewer's join path. Timeout reports incomplete cleanup and may leave
a detached worker/reaper; it is not permission to start a replacement session.

Automatic reconnect remains disabled. Clipboard shutdown still has unbounded joins;
user-close intent must be latched before fallible input flushes, and SSH exit 255
cannot distinguish network loss from authentication/host verification failure.
These need lifecycle work before safe retry. The escape shortcut only releases
input grab; it does not close the session and is not treated as cancellation.
