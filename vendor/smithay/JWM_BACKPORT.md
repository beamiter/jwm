# Smithay touch cancellation backport

Source: https://github.com/Smithay/smithay/tree/d33a9c98fbd4d0ddd85315f93d436747bf3393a6

License: MIT, preserved in LICENSE.txt. Package version is unchanged. This tree contains the exact tracked upstream snapshot plus the documented local change and its regression evidence; it is not an upstream release or a claimed upstream fix.

Local change: `src/input/touch/mod.rs`, `TouchInternal::cancel`. Cancellation creates a frame marker even after a previously framed batch, retires every active slot and deferred up-frame target, emits cancellation once per involved client target using the existing marker mechanism, and removes the marker. Previously a framed sequence had no pending marker, so cancellation returned without revoking it; unchanged slots and up-pending targets were also retained.

This correction supports JWM's modal input isolation. It does not bypass authentication or change host input/security settings. The source-extracted old/new sequence tests are evidence about this state transition, not physical touchscreen or native compositor validation. JWM's permanent touch protocol regressions and full CI are required as integration gates.

Only this fixed revision is patched via the root Cargo.toml Git-source patch. Do not change the version to suppress an advisory or replace it with an unrelated update. Review any future upstream update before removing or rebasing this patch.

Additional non-behavioral vendoring compatibility: `src/backend/allocator/swapchain.rs`, only a local `#[allow(deprecated)]` on `submitted`. Newer Rust deprecates `AtomicU8::fetch_update`; the pinned source still declares Rust 1.87 (root JWM 1.89). Retain the original API and atomic behavior rather than adopting a newer API or relaxing project warnings. Revisit this attribute on an upstream update.

Permanent public-API regression suite: root `tests/smithay_touch_cancel.rs`, run by the ordinary `cargo test --locked --lib --bins --tests` gate when `wayland-backends` is enabled. It uses actual `TouchHandle`/`SeatState` with recording targets; no display socket, fake physical input, or native session is needed. Private marker/slot assertions are represented by no subsequent frame/motion/up delivery and a successful new sequence.
