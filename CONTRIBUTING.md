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

```bash
cargo build --locked
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets --no-deps
cargo test --locked --lib --bins --tests
```

Run focused tests while iterating, but run the complete commands before opening
a pull request. Portal changes use the independent manifest and scripts:

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
