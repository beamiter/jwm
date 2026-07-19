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

- [ ] Add backend-tagged structured error contexts at display, device, renderer,
      and IPC boundaries. Started: `BackendError` carries an optional
      `[backend/boundary] operation` context with the original error preserved
      through `source()`. Tagged so far: backend construction and
      window-manager selection (display), control-socket binding (ipc),
      libseat/udev/libinput/KMS-output startup in the udev backend (device),
      and GPU-compositor initialization in both X11 transports (renderer).
      Remaining: per-operation adoption inside frame production, capture, and
      hotplug paths.
- [ ] Add a deterministic nested-backend smoke matrix covering startup, IPC
      health, config reload, basic window lifecycle, screenshot capture, and
      clean shutdown.
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
full-backend mock. Recording completion and overview/expose are next.

Exit criteria: a core policy test can use small fake capabilities instead of a
mock implementing the complete backend surface. (First met by
`capture_plan::tests::execution_prefers_region_capture` and neighbors.)

## Phase 3 — backend consolidation

- Extract protocol-free X11 behavior shared by X11RB and XCB.
- Move platform-neutral dirty-region, frame-timing, animation, color, and effect
  algorithms into compositor-common modules.
- Keep GLX and EGL/GLES resource ownership in explicit platform adapters.
- Add differential tests that feed identical policy events to both X11
  transports and compare observable state.

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
