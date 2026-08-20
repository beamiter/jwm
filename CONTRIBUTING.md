# Contributing to JWM

JWM spans window-management policy, native X11 transports, Wayland protocols,
DRM/KMS, input, and GPU rendering. Small, independently verifiable changes are
much easier to review and backport than broad rewrites.

## Before changing code

1. Read `docs/architecture.md` and preserve its dependency direction.
2. Search existing pull requests and issues for overlapping work.
3. For rendering or latency changes, record the backend, renderer API, driver,
   monitor topology, refresh rates, and benchmark command before editing code.
4. Keep behavior changes separate from file moves unless tests cover the result.

Security-sensitive findings must follow `SECURITY.md` rather than a public issue.

## Build environment

JWM targets Linux and requires the normal X11, Wayland, DRM/GBM, libinput,
libseat, EGL/GL, ALSA, D-Bus, font, and rendering development packages. The
repository toolchain file installs the expected Rust channel, Rustfmt, and
Clippy components when Rustup is used.

The minimum supported Rust version is declared as `rust-version` in
`Cargo.toml` (currently 1.89) and is enforced with the committed `Cargo.lock`
by a dedicated CI job. Raising it is a reviewable change: update `Cargo.toml`,
the CI `msrv` job, and this note together, and state the dependency that
forced the increase.

```bash
cargo build --locked
cargo fmt --all -- --check
scripts/lint-shell.sh
cargo check --locked --all-targets
cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
cargo test --locked --lib --bins --tests
```

The shell gate requires `shellcheck` (CI installs the distribution package)
and discovers executable Bash helpers automatically. It rejects
ShellCheck warning-level findings in every discovered script, so newly added
helpers cannot silently fall outside the CI list.

Those commands validate the main `jwm` package. The workspace also contains
the shared protocol and bar adapters; changes below `crates/xbar_core` must run:

```bash
cargo test --locked -p xbar_core --all-features --all-targets
cargo clippy --locked -p xbar_linux_actions -p xbar_dbus_providers --all-targets --no-deps -- -D warnings
cargo test --locked -p xbar_linux_actions -p xbar_dbus_providers --all-targets
```

The main crate denies Clippy's correctness, suspicious, and performance groups,
so findings in those groups fail the build. Existing style/complexity/pedantic
debt is deliberately non-blocking until it can be removed in reviewed batches;
new broad `allow` attributes are not an acceptable way around the high-signal
gate. A necessary exception belongs on the smallest item and must explain the
invariant that makes it safe.

Run focused tests while iterating, but run the applicable complete commands
before opening a pull request. Portal changes use the independent build
environment and scripts:

```bash
scripts/test-portal.sh
```

## Runtime validation

Prefer a nested backend for routine development so a failed compositor does not
replace the active desktop session:

```bash
cargo run --locked -- --backend wayland-winit --doctor
cargo run --locked -- --backend wayland-winit

# X11 development can also use Xephyr/Xnest as described in src/lib.rs.
```

For backend-specific changes, report every path actually tested. Do not claim
direct DRM/KMS validation based only on a nested Wayland run, or XCB validation
based only on X11RB.

Useful diagnostics include:

```bash
jwm --backend <backend> --doctor --json
jwm-tool health --json
jwm-tool capabilities --json
jwm-support --backend <backend> --output jwm-support.json
```

## Tests and design expectations

- Add deterministic unit tests for policy, parsing, geometry, state transitions,
  resource limits, and serialization.
- Bound externally influenced buffers, queues, message counts, and timeouts.
- Version persisted or externally consumed JSON schemas.
- Use atomic writes and private permissions for state that may contain session
  information.
- Keep platform-neutral algorithms out of concrete backend modules.
- Avoid new process-global environment reads below the application boundary.
- Document unsafe code with its preconditions and keep the unsafe surface small.
- Never enable the commented release profile as generic cleanup; it requires a
  dedicated benchmark, build-time, and diagnostics review.

## Pull requests

Use a focused title and explain:

- the problem and user impact;
- the chosen design and important alternatives;
- backend and compatibility implications;
- tests and runtime validation performed;
- benchmark results for performance-sensitive changes;
- configuration, IPC, session, or support-bundle schema changes.

Screenshots are useful for visual changes, but they do not replace regression
tests or frame/latency measurements. Mark incomplete work as a draft pull
request and list the remaining validation explicitly.

## Commit hygiene

Write descriptive commits such as `fix(ipc): bound subscription state` or
`perf(x11): reuse damage-region storage`. Avoid large `update` commits in work
intended for review. Keep generated files, editor state, credentials, private
support bundles, and local benchmark captures out of the repository.
