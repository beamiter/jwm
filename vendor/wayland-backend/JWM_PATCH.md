# Local filtered-global lifecycle fix

Base: official crates.io `wayland-backend` 0.3.17, MIT (`LICENSE.txt`).
The downloaded `.crate` SHA-256 is
`38a91b4eaddff87b1cd1074985e3713da4af2c49742d1b356b2c01670a67a078`,
matching the original root Cargo.lock checksum. Its registry VCS metadata names
`Smithay/wayland-rs` commit `72f7fe0d9e99cb720ed57ade0c3077aec533b6ab`,
subdirectory `wayland-backend`.

This is a **local patch**, not a claimed upstream fix. Upstream master inspected
through the GitHub connector still used the same create-before-known-global
ordering. The historical upstream global-filter fixes preserve foreign C-created
globals; this patch preserves that behavior too.

## Problem and change

With `server_system`, `wl_global_create` can synchronously advertise a global
before the returned pointer is inserted into `known_globals`. The old filter
therefore classified this new Rust-managed global as foreign and returned true,
bypassing its visibility callback during the initial broadcast. Binding still
has separate authorization checks; this is not evidence of input delivery to
an unauthorized receiver.

`src/sys/server_impl/mod.rs` is modified and its FFI-free checks are extracted
to the new `src/sys/server_impl/pending_global.rs`. In addition,
`src/sys/mod.rs` gates only its three client-only imports (`Arc`, `wl_proxy`,
`Interface`) with the existing `client_impl` condition
`cfg(any(test, feature = "client_system"))`; `ArgumentType` remains unconditional.
This cfg-only adjustment keeps server-only path builds warning-free without
suppressing lints.

- Scope a pending userdata identity around `wl_global_create`.
- Recognize it only when userdata pointer, display state identity and Rust state
  `TypeId` all match. Retrieve foreign userdata only as an opaque pointer; never
  cast it unless this identity check or the original known-global check passes.
- Run the original `can_view` callback after recognition, without holding the
  display mutex or a mutable TLS borrow.
- Apply the same scope to `wl_global_destroy`, whose global has already been
  removed from `known_globals`, to avoid sending a removal to clients that were
  never allowed to see the global.
- Scoped TLS restores outer identities on nested calls and clears on scope exit;
  null creation returns leave the scope before freeing userdata. No claim is
  made that unwinding across a C callback can be recovered.
- Preserve true foreign-global behavior and the ordinary Rust backend unchanged.
- Require registered, live, exact global identity before lifecycle operations or
  raw global getters. Calls for a creation-in-progress are no-ops (mutation) or
  return InvalidId/None (queries), rather than freeing the creation userdata.
- Clone handler/client-data ownership before invoking reentrant visibility code.
  The handler getter borrows userdata and clones its Arc, never owns its Box.
- Preserve the active dispatch data pointer for nested calls on the same display.
- Add three pure tests for exact typed/display/pointer identity, nested scope
  restoration and registered lifecycle access. The application's real private-Wayland tests exercise dynamic
  target-only seat advertisement, binding and retirement; these require CI.

The base Rust 1.71 manifest/MSRV and dependencies are unchanged. Root Cargo.toml
selects this directory with `[patch.crates-io]`; root Cargo.lock changes only the
wayland-backend source/checksum entries. Registry cache/source files are not
modified. Cargo cache markers, nested lockfiles, build outputs and VCS metadata
are deliberately not included here.

The pure regression harness at `tests/wayland_backend_pending` includes that
same production module by path. It does not disable or claim to run the
upstream unpublished scanner-dependent test suite. The application also has
an owned-display reentrant-global regression for CI; neither suite is evidence
of execution until its actual test result is recorded.
