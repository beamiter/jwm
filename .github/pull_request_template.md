## Problem and impact

<!-- What is wrong or missing? Who notices it? -->

## Design

<!-- Explain the chosen approach, important alternatives, and compatibility implications. -->

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo check --locked --all-targets`
- [ ] `cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings`
- [ ] `cargo test --locked --lib --bins --tests`
- [ ] Changed workspace adapters / bar hosts were tested explicitly
- [ ] Relevant nested backend exercised
- [ ] Relevant native backend exercised, or the gap is explained below

Tested backends and environment:

<!-- X11RB/XCB/wayland-udev/wayland-x11/wayland-winit, renderer, driver, monitors. -->

## Performance evidence

<!-- Required for rendering, scheduling, allocation, latency, or release-profile changes. Otherwise write N/A. -->

## Compatibility and schemas

- [ ] No configuration schema change
- [ ] No IPC schema/command change
- [ ] No session-state schema change
- [ ] No support-bundle schema change

<!-- Document migrations, version changes, deprecations, or rollout details. -->

## Risk and rollback

<!-- What can regress, and how can the change be disabled or reverted? -->
