# JWM evolution roadmap

This roadmap turns broad modernization goals into independently reviewable
changes. It complements the dependency rules in `docs/architecture.md`; it does
not authorize a rewrite or a release-profile change without measurements.

## Delivery principles

1. Keep `cargo check --all-targets`, Clippy, and the library/binary/test suite
   green after every step.
2. Separate behavior changes from structural moves unless regression tests cover
   the combined change.
3. Treat X11RB, XCB, direct Wayland, and both nested Wayland backends as distinct
   compatibility surfaces.
4. Prefer bounded queues, timeouts, versioned schemas, and atomic private state
   for every control-plane feature.
5. Require benchmark evidence for rendering-path changes. The commented release
   profile remains disabled until a dedicated benchmark/build/diagnostics review.

## Phase 0 — sustainable project baseline

- [x] Versioned live health, capability, doctor, and session schemas.
- [x] CI coverage for formatting, all-target compilation, Clippy, and tests.
- [x] Privacy-aware support bundle generation.
- [x] Commit and enforce `Cargo.lock` for reproducible application builds.
- [x] Pin git dependencies to reviewed revisions or release tags.
- [ ] Select and add the project license before publishing releases or crates.
- [x] Establish a documented minimum supported Rust version after testing it.
      Rust 1.89 is the floor imposed by the locked dependency graph, verified
      by `cargo +1.89.0 check --all-targets` and the full test suite, declared
      as `rust-version` in `Cargo.toml`, and enforced by the CI `msrv` job.

## Phase 1 — reliability and supportability

- [x] Add backend-tagged structured error contexts at display, device, renderer,
      and IPC boundaries. Started: `BackendError` carries an optional
      `[backend/boundary] operation` context with the original error preserved
      through `source()`. Tagged so far: backend construction and
      window-manager selection (display), control-socket binding (ipc),
      libseat/udev/libinput/KMS-output startup in the udev backend (device),
      and GPU-compositor initialization in both X11 transports (renderer).
      The udev backend's runtime paths followed: frame production (DRM
      render/queue/vblank watchdog and post-failure reactivation), every
      capture pipeline (screenshot, screenshot-region, wlr-screencopy,
      ext-image-copy-capture output and toplevel), and the hotplug/control
      boundaries (rebuild-outputs on udev events, VT-switch and session
      activation — the last of which previously discarded its error — plus
      DPMS, gamma, and output configuration, whose tagged reasons now flow
      into output-management transaction failure records without changing
      field classification). The shared X11 compositor completed the item:
      `X11ConnectionOps::backend_name` lets the transport-generic compositor
      tag contexts with the live transport (`x11rb`/`xcb`), frame production
      (make-current, transition/presented-scene FBO allocation, native
      texture sync, pixmap rebinding, buffer swap — renderer) and the
      fullscreen unredirect/Present protocol paths (display) log
      per-operation contexts, and every capture path (screenshot,
      screenshot-region, window thumbnails — including previously silent GL
      allocation failures) is tagged. `save_png_async` now carries the
      requesting backend's context so asynchronous encode/rename failures
      stay attributable after the capture call returns.
- [x] Add a deterministic nested-backend smoke matrix covering startup, IPC
      health, config reload, basic window lifecycle, screenshot capture, and
      clean shutdown. `jwm-tool nested-smoke` boots `wayland-winit` and
      `wayland-x11` inside a private `XDG_RUNTIME_DIR`, drives every step
      over the real IPC socket with bounded per-step timeouts from a pure,
      unit-tested matrix table, and emits a versioned JSON report
      (schema_version 1). A failing step preserves exactly one log bundle
      and names it in the report; missing optional tooling (lifecycle
      client, grim) records a skip instead of a false failure. The first
      runs surfaced and fixed two real defects: a shutdown segfault in the
      winit backend (EGL unbound the Wayland display after it was dropped —
      the field-order fix the X11 nested backend already carried), and
      capture globals advertised by nested backends that never service
      them, which made grim hang forever (capture globals are now gated on
      `frame_capture_supported`, so nested sessions report the capability
      honestly and capture clients fail fast).
- [x] Add bounded log rotation or journald guidance for the optional supervisor.
      The daemon log is rotated at a 1 MiB bound into a single previous
      generation (at most two files), and tools/README.md documents the
      journald alternative via a systemd user unit.
- [x] Record support-bundle fixtures and schema compatibility tests. The
      version-1 contract is frozen by `tests/support_bundle_schema.rs`: a
      generated offline bundle and the recorded fixture in
      `tests/fixtures/support_bundle_v1.json` must both satisfy the same
      checker, and sentinel values verify the documented privacy guarantees.
- [x] Add crash-safe state migrations before changing the session schema.
      `migrate_session_json` version-probes a snapshot, migrates version 1
      through a tolerant representation with normalization instead of
      rejection, refuses unknown versions without partial state, and never
      rewrites the on-disk file during load; recorded v1/v2 fixtures in
      `tests/fixtures/` freeze both generations for the future v3 migration.

Exit criteria: a failed startup or smoke test produces one actionable report
without requiring an unbounded debug log or a reproduction on direct DRM/KMS.

## Phase 2 — capability-oriented architecture

Continue splitting `backend::api::Backend` into focused interfaces:

- window lifecycle and focus;
- input and bindings;
- output discovery and configuration;
- render scheduling and presentation;
- capture/media;
- compositor effects and diagnostics.

Move feature-owned state from `Jwm` into explicit services, beginning with
screenshot/recording, overview/expose, magnifier, and session persistence. Keep
transport-specific IDs behind adapters and pass typed events across boundaries.

Started: interactive screenshot completion is the first extracted policy
service (`jwm::features::capture_plan`). The completion decision is a pure
function, capture execution depends only on the `CompositorMedia` capability,
and its tests exercise the exit criteria below with a small fake instead of a
full-backend mock. Screen-recording policy followed in
`jwm::features::recording_plan`: region normalization (encoder-aligned even
dimensions), output-path validation, the output-directory fallback chain, and
segment finalization planning are pure and tested; `toggles.rs` and the IPC
handler only execute the returned plans. Overview navigation followed in
`jwm::features::overview_plan`: the prism sliding-window rule — previously
implemented three times with one drifting copy — has a single tested
implementation, and cycling returns a plan (rotate versus refresh-subset)
that the orchestration executes. Expose followed in
`jwm::features::expose_plan`: window eligibility (visible, positive size,
non-empty set) and the enter/exit/click/escape decisions are pure
`ExposeAction` plans, and the exit sequence — previously duplicated four
times across `toggles.rs` and `input_handler.rs` — has a single executor
(`apply_expose_action`). The magnifier already meets the bar: its state and
source-rectangle math are a pure, tested module (`features::magnifier`) with
orchestration confined to the input handlers.

Exit criteria: a core policy test can use small fake capabilities instead of a
mock implementing the complete backend surface. (First met by
`capture_plan::tests::execution_prefers_region_capture` and neighbors.)

## Phase 3 — backend consolidation

- Extract protocol-free X11 behavior shared by X11RB and XCB. Started: the
  request-flush batching policy — pending-op and elapsed-time thresholds
  and their load-adaptive tuning — was byte-for-byte duplicated in
  `x11rb/batch.rs` and `xcb/batch.rs` (the x11rb copy even carried a
  "keep semantics identical to the native XCB backend" comment, i.e. a
  manual-sync burden). It now lives once in
  `backend::x11::wm::batch::BatchCounters`, a transport-neutral,
  unit-tested coordinator: `note_op` decides whether a flush is due,
  `on_flushed` resets, and each transport keeps only its own
  `conn.flush()`. The x11rb/xcb differential confirmed the two transports
  produce identical observable state before and after the move.
  Two more protocol-free decisions followed: the EDID-HDR-caps → compositor
  colour-settings mapping (PQ-over-HLG precedence, BT.2020, peak nits,
  10-bit) is now the pure, tested `backend::edid::hdr_compositor_plan`
  applied via `Compositor::apply_hdr_plan`, and the background-pointer
  output resolution / screen-layout cache invalidation is the tested
  `backend::x11::wm::enrich_background_event` — the latter had already
  drifted (the x11rb copy carried a dead `ButtonRelease` arm the xcb copy
  had dropped). Both backends now call the single implementation.
  The primary-monitor refresh derivation followed as the pure, tested
  `wm::primary_refresh`, and consolidating it surfaced a real unit bug:
  both transports' runtime compositor-toggle paths fed `Compositor::new`
  raw RandR millihertz (60000 for "60 Hz") through per-backend helpers
  whose comments claimed Hz, skewing blur tiers and refresh seeding —
  startup had always converted correctly. The seven compositor capability
  traits (benchmark, diagnostics, control, media, workspace effects,
  window effects, annotation) were ~500 hand-written forwarding lines per
  transport, drifting in method order and parameter names; both backends
  now generate them from
  `wm::compositor_delegation::delegate_compositor_capabilities!`, and the
  source-parity test asserts the impls come from the macro rather than
  diffing two hand-written blocks. The backend-event → compositor bridge
  (`compositor_handle_event`, two parallel ~130-line matches) followed the
  plan/executor pattern: `wm::event_bridge::compositor_event_ops` is the
  pure, tested decision (root/overlay exemptions, fullscreen add/toggle
  mapping, class guards, present/damage forwarding), `Compositor::
  apply_event_op` is the single executor, and the transport-specific
  root-geometry query moved behind the new
  `X11ConnectionOps::query_window_size`, leaving each backend a thin
  closure-wiring wrapper.
- Move platform-neutral dirty-region, frame-timing, animation, color, and effect
  algorithms into compositor-common modules. Started: the event coalescer,
  workspace-transition timing, and wobbly-window simulation — std-only
  algorithms previously homed in the X11 tree and imported from policy and
  Wayland code — now live in `backend::compositor_common`, with the X11
  namespace keeping compatibility re-exports for its own tree. Architecture
  boundary tests now reject new policy or Wayland imports of
  `x11::compositor_common`. The expose grid layout and animation followed:
  the Wayland tree carried a drifted inline copy (missing division guards,
  no accelerated fade-out once geometry converges); both backends now
  instantiate the single implementation in
  `backend::compositor_common::expose` over their native window-id types.
- Keep GLX and EGL/GLES resource ownership in explicit platform adapters.
  Started: the graphics platform is now a directory module with one adapter
  per API — `compositor/platform/glx.rs` owns the GLX context, overlay
  drawable, TFP entry points, and GLXPixmap lifecycle;
  `compositor/platform/egl.rs` owns the EGL display/context/surface, the
  GLES library handle, EGLImage lifecycle, and the damage/buffer-age entry
  points — with the API-selection facade and recording-cursor compositing
  in `platform/mod.rs`. GLX symbol resolution was consolidated en route:
  the OML sync-control manager previously called glXGetProcAddress itself;
  `GlxPlatform::load_oml` now resolves those entry points and hands the
  manager ready function pointers.
- Add differential tests that feed identical policy events to both X11
  transports and compare observable state. Started: the nested smoke
  matrix now boots `x11rb` and `xcb` inside private Xephyr servers and
  drives both through the same fixed 15-stage IPC scenario, capturing a
  normalized observable-state snapshot after each stage — window
  class/tags/floating/geometry and per-workspace layout/occupancy, with
  transport-relative ids and asynchronous titles excluded by
  construction. The scenario exercises the divergence-prone tiling
  surface: a two-window master/stack split, `setlayout`
  (tile/monocle), `setmfact`, `incnmaster` up and down, `zoom`,
  `focusstack`, `movestack`, `togglebar` (workarea change), a tag
  round-trip, and `togglefloating` — 14 of 15 stages produce distinct
  observable state. Both `MapClient` stages spawn the *same* resolved
  client (xterm): a single reliable client used twice keeps the managed
  set deterministic, whereas heterogeneous libXt clients (xclock/xeyes)
  race on transient startup windows and leak client-startup timing into
  the comparison. `jwm-tool nested-smoke` compares the two transports'
  snapshot sequences and fails the matrix on the first divergence,
  naming the snapshot index and section; the normalization and
  comparison rules are pure and unit-tested, and every scenario command
  is checked against the live `jwm::ipc` registry so the matrix cannot
  drift from the compositor. Live runs are stable with x11rb and xcb
  producing byte-identical state across all 15 snapshots.

Exit criteria: new X11 policy features are implemented once, while protocol and
renderer differences remain isolated and independently testable.

## Phase 4 — selectable production builds

Introduce additive Cargo features for backend families only after dependency
boundaries are ready. The default build must remain compatible during migration.
Target profiles should eventually include:

- X11RB + shared X11 compositor;
- XCB + shared X11 compositor;
- direct Wayland DRM/KMS;
- nested Wayland development backends;
- optional portal and media integrations.

Exit criteria: each supported profile compiles in CI, reports its capabilities
accurately, and fails clearly when a disabled backend is requested.

## Phase 5 — performance contracts

Define benchmark scenarios and regression budgets before further renderer
tuning:

- idle CPU and wakeups;
- frame-time median, p95, and p99;
- damage-area and redraw ratios;
- input-to-present latency where timestamps are available;
- allocation counts in steady-state frame production;
- multi-monitor refresh-rate and mixed-scale behavior;
- direct-scanout entry/exit stability.

Store machine-readable baselines with hardware, driver, backend, renderer API,
and configuration metadata. Never compare unlabeled results from different
systems as if they were the same benchmark.

## Phase 6 — release readiness

- Publish signed source archives and checksums from an immutable tag.
- Generate a changelog from reviewed pull requests.
- Package `jwm`, `jwm-tool`, `jwm-support`, desktop sessions, and documentation.
- Validate install, upgrade, config backup, rollback, and uninstall paths.
- Document supported distributions, driver constraints, and known backend gaps.
- Define a deprecation window for IPC, configuration, and session schema changes.

A release is ready when a fresh installation can be diagnosed, upgraded, and
rolled back using documented commands, not merely when a release build succeeds.
