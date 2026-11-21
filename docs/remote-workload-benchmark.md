# Remote workload baseline protocol

This protocol separates **repeatable workload preparation**, **host pipeline
telemetry**, and **physical input-to-display measurement**. The scripts and
fixtures can be tested without a desktop. That does not constitute a dual-machine
benchmark, RTX 5080 validation, Isaac Sim compatibility, or latency acceptance.

## Record before measuring

Use two physical machines, a trusted LAN, and the same clean build for each
comparison. Keep a private run manifest with:

- Source and viewer CPU/GPU, OS/kernel, GPU driver, display server/backend,
  monitor resolution/scaling/refresh rate, and power mode.
- Commit, build profile, host/client options, capture path (X11 desktop or window),
  source/window and encoded dimensions, frame rate, codec/quality, and whether
  the viewer was fullscreen. Record any dimension changes instead of silently
  comparing different resolutions.
- Wired/wireless link, measured available bandwidth and RTT, packet-loss profile,
  application version, workload/scene revision, and exact actions/duration.
- Run ID, warm-up duration, start/end procedure, and whether other processes
  competed for CPU/GPU/network. Record viewer performance separately: host
  telemetry cannot explain every viewer bottleneck.

`python3 scripts/remote-benchmark-metadata.py` provides optional read-only CPU
and GPU inventory; it does not measure GPU utilization or performance. Inspect
its CLI help for privacy controls. Never include credentials or private desktop
content in public evidence. Raw stderr and screenshots can contain private data.

Use one process session/build/workload per log. Warm up for at least 30 seconds,
then capture a fixed 60-second measurement interval, repeated three times.
Exclude warm-up *whole records* consistently; do not prorate a five-second
telemetry window. Record actual retained duration. If restarting changes warm-up
state, keep the session alive and select the steady-state records offline.

## Workloads

### Terminal

Open an actual terminal on the source, hold size/font/scaling fixed, then run:

```sh
python3 scripts/remote-terminal-workload.py --seconds 60 --fps 30 --rows 24 --columns 80
```

This bounded fixture alternates full repaint content, ASCII thin strokes,
Chinese and Greek text. `columns` is a text-character budget, not a guarantee of
terminal-cell width for wide Unicode. The fixture restores cursor/alternate
screen on normal exit and interruption. It requires a TTY and is not a terminal
emulator benchmark. Also measure a separate idle text phase; do not mix idle
compression savings into scrolling frame-rate results.

### Browser

Open `tests/fixtures/remote-workloads/browser.html` locally in the source browser.
It is self-contained and makes no network requests. Measure separate static,
30-second scrolling/motion, and modal-open/close phases. Fix viewport, browser
zoom, and refresh rate. Repeated keys toggle a black/white marker for external
camera work. DOM key handling and animation callbacks are not physical latency
measurements. Include real browser address-bar/menu/focus behavior separately;
this fixture alone cannot validate popup/window capture integration.

### Blender Workbench

Use `--workload blender` for the actual Blender viewport. Pin the Blender
build, scene, camera, renderer, and raster. Observe static detail, distinct
positions during motion, and the return to a stable full-size image after
playback stops. Record the encoded raster and interface edge contrast along
with frame intervals. [Blender scene and observations](benchmarks/remote-upgrade-2026-11-20/README.md#blender-viewport-through-managed-wayland-input)

### Isaac Sim

Run the actual supported Isaac Sim version on the target GPU with a fixed scene,
seed, viewport resolution, simulation rate and render settings. Record launch,
steady simulation, orbit/pan, and UI/menu interaction as distinct phases. Save a
scene identifier/hash and action sequence so the next run can repeat it. Do not
replace this with the browser animation or claim cloud fixture tests validate
Vulkan presentation, GPU synchronization/readback, or NVIDIA drivers. If the
hardware/application is unavailable, mark this row **not measured**.

For each workload, take native-resolution source/viewer screenshots after at
least 500 ms of stillness and inspect text edges, color, thin lines, stale tiles,
clipping, and transient/modal contents. Preserve the exact encode settings.
Automated image comparison is useful only with aligned frames and known scaling.

## Collect and analyze telemetry

Enable `--stats-json` on the documented `jwm-remote host` invocation and save its
stderr privately. See [remote-control.md](remote-control.md#machine-readable-pipeline-timings)
for the wire-compatible telemetry schema and stage definitions. For example,
after collecting steady-state host and viewer logs:

```sh
python3 scripts/remote-workload-report.py \
  --telemetry terminal-run1-host.stderr --viewer-telemetry terminal-run1-viewer.stderr \
  --workload terminal --run-id terminal-run1 \
  > terminal-run1.report.json
```

Enable `--stats-json` on the viewer to collect its separate decode, completion
and input queue stages. With `ssh --stats-json`, supply the same combined log
to both arguments. The report pairs the ends by authenticated session ID and
reports counts for unmatched sessions. Older host-only logs can be analyzed with
`--telemetry` alone. The parser keeps typed schema-1 windows and validates
bounded input, build identifiers, counts, timing policies and numeric values.

Use `jwm-remote session --stats-json` to collect raw viewer windows on stderr
alongside the managed session events on stdout. Native Wayland viewer windows
can include `native_wayland_stages`. Its stages measure VA surface wait and
frame export (`va_surface_wait_ms`, `va_frame_export_ms`), RGB target allocation
(`rgb_target_allocation_ms`), EGL preparation and submission through fence
creation (`egl_prepare_and_submit_ms`), fence wait (`egl_fence_wait_ms`), and
the per-buffer Wayland DMA-BUF import request and reply
(`wayland_dmabuf_import_ms`). These timings are already included in
`native_preparation_and_submit_ms` and cover work the viewer performs before
presentation feedback arrives.

Native X11 viewer windows can include `native_x11_stages` with the same
VA and EGL stage definitions. X11 separates DMA-BUF import
(`x11_dmabuf_import_ms`), picture setup (`x11_picture_setup_ms`), the
letterbox clear and copy or scaled composite into the clean backing
(`x11_backing_populate_ms`), and submission of the window copy, cursor
and completion request (`x11_window_submit_ms`). A report omits stages
with no observations. XRender setup on a standalone resize redraw can
occur outside these per-frame windows. The total native preparation and
submission already includes the measured phases.
With X11 preallocation enabled, `x11_prefetch_rgb_allocation_ms` records
worker time for fresh targets used by a native submission. These observations
occur on a separate thread outside the native submit duration. Count them
alongside synchronous `rgb_target_allocation_ms` observations to track how
many submissions used each allocator.

Report interpretation:

- Throughput uses the sum of actual `window_seconds`, including partial windows.
  Payload Mbit/s excludes transport overhead and is not measured link capacity.
- Weighted means are computed only when the schema exposes the matching
  observation count: capture/captured, writer and socket/sent, ACK/drawn ACKs.
  Other whole-run means remain null instead of inventing denominators.
- When skip counters are present, `totals` sums the recorded values and
  `counter_windows` identifies how many windows carried each counter. For
  X11 desktop timing, `x11_capture` covers only windows with the corresponding
  object and weights each stage by its explicit `observations` count.
- Each stage in `viewer.native_wayland_stages` reports the number of windows
  carrying it and weights its mean by the total `observations` in those
  windows. The group appears only when native Wayland stages were recorded.
- The X11 VA-API direct map stage reports the read-only map and copy into
  padded NV12 staging. `viewer.hardware_rgb_work.vaapi_map` summarizes the
  windows carrying mapped frames, staged bytes and fallback frames. Ordinary
  downloads, including fallbacks, remain part of the download totals.
- `x11_cpu_prepare_ms` covers completed worker jobs, including work discarded
  after a source or geometry change. `x11_presenter_submit_ms` stops when the
  X11 draw call returns. `presentation_completion_ms` extends through the
  completion event. Use the queue stage to see time a decoded image spent
  waiting for preparation and submission. The preparation counts can cross
  window boundaries.
- Stage quantiles cover the **most recent 128 observations per stage per window**.
  `max_reported_window_recent_p95_ms` is the largest such window p95, not a
  whole-run p95. Whole-run percentiles remain null. Archive raw observations if
  a later instrumentation version supports exact run-level percentiles.
- Stages overlap and have different denominators. Do not sum their means.
  Capture-start-to-ACK includes capture and viewer drawing; send-to-ACK also
  includes viewer work. Neither is network RTT or physical input-to-display.
- The X11 direct path exposes separate core `GetImage` and MIT-SHM timing
  spans. Each runs from the request through the reply. Separate platform
  traces can distinguish GPU synchronization from data transfer inside
  those spans and identify encoder copies.

## Physical latency and acceptance

Use an external high-speed camera framing the physical input trigger and viewer
response, or an independently calibrated input/photodiode apparatus. Document
what marks physical actuation (for example a hardware-trigger LED), scanout
location, camera frame rate/exposure, frame-count method and uncertainty. A
browser timestamp, injected key event, frame ACK, or screenshot is not a valid
substitute. At 240 fps, one frame is about 4.17 ms; disclose quantization and do
not report precision finer than the measurement supports.

Take at least 100 attempts per trial and three independent trials for each
workload and configuration. Retain all outcomes and outliers. Start the
operator-measured CSV with this header.

```csv
trial,sample,latency_ms
```

Rows use non-personal trial IDs and unique positive sample IDs per trial. In
`latency_ms`, enter finite nonnegative measured milliseconds, `timeout`, or
`no_response`. Add `--physical-csv measured.csv` to the report command to
report both attempt and measured counts, unresolved sample IDs, and the
nearest-rank p95 for each trial. The p95 ranks unresolved attempts after all
measured values, so a finite result is an upper bound when a trial has
unresolved attempts. When the p95 falls on an unresolved attempt, the report
shows `null` and the 50 ms target remains unverified. The sampling check uses
all attempts. `measurement_method_verified` remains `false` pending evidence
of the apparatus and methodology.

Maintain a result matrix for terminal, browser, and the selected 3D workloads.
Include source/viewer configuration, FPS, payload rate, capture/encode/write/ACK timing, physical p95
and uncertainty, static image quality, interaction failures, and evidence paths.
Mark missing measurements **not measured**, not zero or pass. Compare competitors
only under the same hardware, workload, resolution and measurement procedure.
Network degradation tests require a separately authorized network setup; these
scripts do not alter system/network settings. Repeat the matrix under each
agreed bandwidth/RTT/loss profile rather than extrapolating from the wired LAN.

## Cloud regression checks

```sh
python3 scripts/test-remote-workload-report.py
python3 -O scripts/test-remote-workload-report.py
python3 scripts/test-remote-benchmark-metadata.py
```

These checks cover fixture generation, bounded parsing, aggregation, missing
sample handling and physical-CSV bookkeeping. They do not run real two-machine
capture, a browser renderer, Isaac Sim, or a high-speed camera. Physical baseline
rows remain pending until their evidence is collected on the target machines.

## Versioned observations

The [2026-11-19 X11 SSH benchmark](benchmarks/remote-x11-ssh-2026-11-19/README.md)
records live desktop streaming, two runs of isolated X11 workloads on the same
machines, and software-observed input and edge release latencies.

The [X11 comparison with Sunlogin](benchmarks/remote-x11-sunlogin-2026-11-19/README.md)
uses one physical monitor and one synthetic workload across the two products.
It records the additional JWM display-power trials and the host pipeline
timings behind the observed gap.
