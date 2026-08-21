# JWM architecture

JWM is split into a process shell, an application composition root, window
management policy, platform backends, and reusable state/layout code.

```text
src/main.rs                 process setup (CLI, logging, locale, D-Bus)
    |
    +-------> src/doctor.rs read-only startup diagnostics
    |
    v
src/application.rs          backend selection and application lifecycle
    |
    +-------> src/jwm/      window-management policy and use cases
    |              |
    |              v
    +-------> src/core/     state, models, layout and animation
    |
    v
src/backend/api.rs          platform boundary
    |
    +-- x11rb / xcb / X11 compositor
    +-- Wayland udev / X11 / winit

tools/jwm_remote.rs         separate trusted-LAN X11 helper
    |
    +-------> src/remote/   authenticated transport, JPEG viewer, XTEST
                    |
                    +-----> X Composite overlay shared by x11rb / xcb JWM
```

## Dependency rules

1. `main` may depend on `application` and OS process services only. It parses
   process inputs into an immutable `ApplicationOptions` snapshot before the
   display server or worker threads start.
2. `application` is the composition root. Concrete backend constructors belong
   here; policy code must not select a concrete backend.
3. `jwm` implements window-management behavior against `backend::api::Backend`.
4. `core` contains state and deterministic policy. New core code should avoid
   concrete backend modules; platform-neutral IDs and events should gradually
   move from `backend` into `core`.
5. Backend implementations may depend on `core` and compositor-common code,
   but must not call JWM feature modules directly. Events cross the boundary
   through the backend event-handler interfaces.
6. `remote` is an out-of-process X11 client, not part of local IPC. Its only
   narrow compositor coupling is an X11 capture-owner lease observed by both
   backends to inhibit fullscreen unredirect while the Composite overlay is
   being read. Network messages must remain a closed screen/input protocol and
   must never forward arbitrary `jwm-tool` commands into the compositor.

## Current hotspots

- `backend/api.rs` is a broad interface. Split it by capability (windowing,
  input, outputs, rendering, capture) as implementations are migrated.
- `jwm::Jwm` owns both durable WM state and infrastructure caches. Move feature
  state behind focused services before adding more fields.
- X11RB and XCB backends duplicate substantial behavior. Extract protocol-free
  operations into `backend/x11` and retain only transport adapters in each.
- X11 and Wayland compositor trees contain parallel render/effect modules.
  Prefer `backend/compositor_common` for platform-neutral algorithms.
- `config.rs` is large and process-global. Separate schema, loading, validation,
  defaults and live-reload; pass an immutable configuration snapshot inward.

## Incremental migration order

1. Keep `cargo check --all-targets` and unit tests green as the safety baseline.
2. Introduce typed application events and split the backend API by capability.
3. Move JWM feature state into screenshot, recording, overview and magnifier
   services with explicit inputs/outputs.
4. Consolidate X11 transports and compositor-common algorithms.
5. Gate concrete backends with Cargo features so a production build compiles
   only the selected platform stack.

## Migration status

- Application composition root extracted from the process bootstrap.
- Startup backend and benchmark settings are represented by typed
  `ApplicationOptions` instead of being rediscovered from environment variables
  inside the lifecycle loop. Environment variables remain a compatibility
  adapter at the process edge.
- Configuration generation, path discovery and validation are exposed through
  application-level maintenance operations, so the binary does not reach into
  configuration implementation details.
- Configuration semantics now live in `config/validation.rs` as structured,
  serializable diagnostics shared by startup preflight, `--check-config`, the
  doctor, live reload, and IPC status. Blocking errors never replace the active
  runtime snapshot; generated files are atomically written and symlink-safe.
- Live reload combines backend-specific inotify notifications with a
  backend-neutral, low-frequency revision poll. Both paths share debounce and
  revision deduplication, so nested backends reload reliably without repeatedly
  parsing the same malformed edit.
- IPC command arguments use checked conversions and reject malformed shapes,
  empty spawn commands and numeric overflow instead of silently defaulting or
  narrowing. The CLI propagates server failures through its exit status.
- The IPC endpoint validates private runtime directories, preserves active
  instances and reclaims only unchanged stale sockets. Per-client buffering,
  connection intake and subscription state are bounded so control traffic
  cannot monopolize the compositor loop.
- Versioned runtime health and capability snapshots expose the actual selected
  backend and supported control surface without changing legacy IPC envelopes.
- Session snapshots use an atomic private state store, validate schema and
  payload limits, and restore monitor/tag placement with on-screen floating
  geometry while continuing to read the legacy cache location.
- `EventHandler` explicitly delegates immediate compositor rendering to JWM's
  render pump. X11 Damage events no longer fall through the trait's no-op
  default and wait for the periodic update tick.
- Startup health checks are represented as a versioned `DoctorReport`, keeping
  environment and filesystem inspection independent of display construction
  and machine-readable for support tooling.
- Restarts preserve the original OS-native argument vector and resolve the
  current executable once, avoiding repeated UTF-8 conversions and allocations.
- The experimental `[profile.release]` tuning block remains deliberately
  commented and reference-only. It must not be enabled without explicit
  maintainer approval and a dedicated benchmark/build/diagnostics review.
- Interactive move/resize transport state moved into the backend contract.
- Key-repeat policy is binding metadata produced by configuration; the Wayland
  input backend no longer inspects or imports concrete `Jwm` methods. Udev
  repeat is event-driven: a repeatable press owns a cancellable 400 ms/50 ms
  calloop timer, while config generations, exact modifiers, session locks and
  VT state prevent a stale timer from emitting into a new action or lock
  surface.
- Wayland xdg-toplevel configure fallback uses one 250 ms one-shot per new
  surface rather than a permanent 50 ms poll. `wp-commit-timing` and `wp-fifo`
  remain explicitly unmanaged: timestamps are consumed for protocol progress,
  no Smithay barriers are installed, and the compositor does not scan the
  scene for nonexistent barrier state.
- X11RB/XCB share one calloop pacing policy: continuous handler/compositor work
  selects a 16 ms adaptive update cadence, X damage renders immediately after
  dispatch, timer-driven updates suppress a redundant post-dispatch swap, and
  recording contributes its own deadline. The 20 ms idle maintenance fallback
  remains explicit until clipboard delivery and lifecycle/telemetry work gain
  readiness or exact deadlines.
- JWM owns a process-lifetime level-triggered epoll hub registered once by each
  X11 loop. It nests IPC's listener/client epoll and dynamically added per-bar
  command eventfds, so IPC reconnects and monitor hotplug do not mutate calloop
  registrations. IPC uses an eventfd for userspace-only fairness continuation
  and conditional `EPOLLOUT`; xbar's command notifier preserves the futex wire
  protocol while bridging bar-to-WM commands to eventfd. Source eventfds and
  rings are drained before the aggregate ready list, and bar teardown removes
  its interest before destroying the ring and joining the waiter thread.
- Compositor benchmarking is the first capability extracted from the monolithic
  `Backend` trait. Application startup now depends on `CompositorBenchmark`
  rather than the complete platform interface for benchmark configuration.
- Read-only compositor and protocol telemetry now lives in `BackendDiagnostics`:
  performance, direct scanout, presentation, capture, XWayland, session lock,
  tearing hints, color management and protocol-bind snapshots are separated
  from commands that mutate backend state.
- Compositor-wide visual mutations now live in `CompositorControl`: color
  temperature, saturation, brightness, contrast, inversion, grayscale, debug
  HUD, transition mode, WaterLily toggle and live config application no longer
  expand the core `Backend` method set.
- Capture and media workflows now live in `CompositorMedia`: full/region
  screenshots, static/live thumbnails, recording lifecycle and audio timing
  are isolated from window-management and general compositor controls.
- Workspace transition effects are moving into `CompositorWorkspaceEffects`.
  Tag transitions, magnifier state, snap-preview lifecycle, overview/expose and
  monitor-layout synchronization are isolated behind backend-specific ID and
  refresh-rate adapters.
- Per-window visual state is moving into `CompositorWindowEffects`. Frame
  extents, shaped-window flags, urgency, picture-in-picture, wobbly movement,
  pointer/edge-glow effects, dock targets, peek, tab groups and zoom-to-fit are
  isolated behind native window-ID adapters.
- Accessibility color correction and interactive screen drawing now live in
  `CompositorAnnotation`, separating annotation stroke state from the general
  backend lifecycle.
- Output hardware queries and mutations now live in `DisplayControl`, covering
  VRR capabilities/toggles, KMS color-pipeline capabilities and HDR metadata.
- Lightweight render scheduling now lives in `RenderScheduler`: render requests,
  compositor presence, pending-render state and overlay identity are separated
  from frame production and compositor resource initialization.
- `BackendError` supports backend-tagged structured contexts
  (`[backend/boundary] operation`, boundary ∈ display/device/renderer/ipc)
  while preserving the original error through the `source()` chain. The
  application composition root tags backend construction and window-manager
  selection at the display boundary, and IPC socket-bind failures are tagged
  at the IPC boundary. Inside the backends, udev startup tags libseat, udev
  enumeration, libinput seat assignment and the initial KMS output scan at the
  device boundary, and both X11 transports tag GPU-compositor initialization
  at the renderer boundary. New failure paths that cross the platform boundary
  should attach a context instead of stringifying the underlying error.
- Interactive screenshot completion is extracted into the
  `jwm::features::capture_plan` policy service: the completion decision
  (cancel / too-small / capture, clipboard staging, annotation baking) is a
  pure function, and capture execution depends only on the `CompositorMedia`
  capability so tests use a small fake instead of mocking the full backend.
  `Jwm::finish_screenshot_select` now only tears down interaction state and
  routes the plan's outcome to logging, clipboard, and annotation baking.
- Screen-recording policy lives in `jwm::features::recording_plan`: initial
  region normalization (clamping plus even encoder alignment), output-path
  validation, the output-directory fallback chain, and the segment
  finalization plan (validate-in-place, move for legacy callers, or ffmpeg
  concat) are pure functions shared by the key-binding and IPC entry points.
  The orchestration keeps only filesystem side effects, ffprobe/ffmpeg
  execution, and compositor calls.
- The event coalescer, workspace-transition timing, and wobbly-window
  simulation moved from the X11 namespace into `backend::compositor_common`,
  their platform-neutral home. `x11::compositor_common` re-exports them so
  the X11 tree keeps its paths, while the policy layer and the Wayland
  backends import the canonical location; `tests/architecture_boundaries.rs`
  now enforces both directions (no `x11::compositor_common` outside the X11
  tree, and no `backend::x11::` imports from the policy layer at all).
- Overview navigation policy lives in `jwm::features::overview_plan`. The
  prism sliding-window rule (`window_start`) is the single canonical
  implementation used by `OverviewState`, initial activation and cycling,
  replacing three divergent copies. `plan_activation` aligns the focused
  client, state index and first bounded six-window GPU subset;
  `plan_cycle` decides whether a navigation step only rotates the prism or
  must refresh it with a new subset. The matching protocol-free camera,
  regular-polygon construction and painter ordering live in
  `backend::compositor_common::prism` and are consumed by both render
  backends. The Wayland GLES adapter preserves that ordering across live and
  filler faces plus generated polygon caps, so missing textures and the spare
  slots of one- or two-window prisms remain closed without adding renderer
  state to the shared geometry. Its mirrored pass builds a second shared piece
  stream from `mirror_matrix(floor) * base_model`; the solid and reflection are
  therefore sorted independently without a depth buffer. An overview-only
  static skydome program owns the horizon and floor-light environment, leaving
  the simpler Expose/Peek background program untouched and avoiding idle-frame
  scheduling. Solid and reflected live faces share the ordinary window's
  per-surface color-transform plan. With a live FP16 target, described surfaces
  map into one normalized linear-sRGB workspace independently of output
  overlap. The overview therefore stays in common linear light as it crosses
  outputs; output gamut and transfer are applied only after its complete
  painter-sorted stream. Linear-tail-safe frames use either per-output software
  regions (nonnegative physical origin, unit scale, normal transform and no
  conflicting overlap) or one coherent all-output CRTC CTM+GAMMA_LUT pair.
  Encoded-only late overlays, capture, KMS-external cursor/drag/lock/top/overlay
  elements, unsupported topology or a missing FP16 target select the global
  sRGB fallback for the whole frame. A normal pointer usually intersects an
  active output, so current interactive desktop frames mostly take that
  fallback; per-output live delivery remains infrastructure until external
  elements gain color adapters. DRM HDR signalling is therefore fail-closed:
  enabling `HDR_OUTPUT_METADATA` is rejected for now, inherited metadata is
  cleared at KMS ownership boundaries, and the runtime target remains exact
  sRGB. Status IPC reports EDID profiles as capabilities rather than active
  output signals and has no last-frame delivery-route snapshot. The GLES adapters
  transform straight color
  (unpremultiply, decode, gamut matrix, optional encode, repremultiply), retain
  explicit PQ/HLG decode plans when entering the common workspace, and normalize
  runtime 3x3 uploads to column-major data with `GL_FALSE`; filler and legacy
  transition branches explicitly clear that state. The workspace is relative,
  not absolute-luminance-normalized; non-D65 descriptions have no chromatic
  adaptation, dynamic surface-description changes are not yet latched to the
  corresponding `wl_surface.commit`, KMS-external elements are not adapted,
  and color properties plus framebuffer are not committed as one atomic
  transaction. All close paths go
  through `OverviewState::deactivate`, which also resets the slide offset the
  inline Escape path used to leave stale.
- Session snapshots load through an explicit version-probed migration
  (`session::migrate_session_json`): version 1 parses through a tolerant
  representation whose invalid floating state is normalized rather than
  rejected, unknown versions fail without partial state, and loading never
  rewrites the on-disk snapshot, so a crash mid-restore or a downgrade always
  finds the previous file intact. Recorded v1/v2 fixtures freeze both
  generations before any future schema change.

Each step should be behavior-preserving and land independently. Avoid moving a
module and changing its behavior in the same change unless tests cover it.
