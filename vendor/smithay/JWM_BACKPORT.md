# Smithay pinned snapshot with JWM backports

Base: https://github.com/Smithay/smithay/tree/d33a9c98fbd4d0ddd85315f93d436747bf3393a6

License: MIT, preserved in LICENSE.txt. Package version is unchanged. This tree contains the exact tracked upstream snapshot, a set of unmodified upstream bug-fix commits cherry-picked from later upstream history, and one documented local change with its regression evidence. It is not an upstream release or a claimed upstream fix.

`JWM_BACKPORT.patch` is the complete difference from the base revision (`git diff d33a9c98..` of the vendored tree, excluding these two notes). Applying it to a pristine checkout of the base reproduces this directory.

## Upstream backports

Each commit below is taken verbatim (`git cherry-pick -x`, no conflicts) from upstream history between the base and `24647fc8d00131eb7f388efed26e35ea316b0018` (2026-10-20), in upstream order:

| Upstream commit | Change |
| --- | --- |
| `79bbed5e` | GLES cache cleanup/invalidation only makes the EGL context current when resources are queued, so idle GPUs are not woken. |
| `0decc2ba` | xdg-decoration posts `invalid_mode` for an unknown `set_mode` value. |
| `30eb0c86` | `DrmDeviceFd::new` no longer tries to become DRM master on a render node. |
| `bc21b828` | `xdg_toplevel.set_min_size`/`set_max_size` with a negative dimension posts `invalid_size`. Previously the value reached `Size::new`, whose negative-size assertion aborts debug builds. |
| `5a7e248c` | `wl_keyboard.modifiers` is sent after the `key` event that changed them, matching GNOME (Smithay issue #2184). |
| `5ed59867` | `ext_image_copy_capture_session_v1.create_frame` with an active frame posts `duplicate_frame`. |
| `118e34ff` | `DrmSurface::use_vrr(false)` succeeds as a no-op on legacy DRM. |
| `496c98b1` | Negative `wl_region.add`/`subtract` sizes are ignored. Previously any client could abort debug builds through the negative-size assertion, and release builds stored inverted rectangles. |
| `506fda05` | Narrow `#[allow(deprecated)]` for `AtomicU8::fetch_update` (MSRV 1.87). Replaces the earlier JWM-local attribute. |
| `f94618d9` | XWM: remove a dead FocusOut handler. |
| `a6a3fa41` | XWM: clear `_NET_ACTIVE_WINDOW` the way the X server does. |
| `1fcc4850` | XWM: ignore `FocusIn(NotifyPointer)` when tracking `_NET_ACTIVE_WINDOW`. |
| `deede709` | XWM: ignore grab/ungrab-mode `FocusIn` when tracking `_NET_ACTIVE_WINDOW`. |
| `904d3bda` | input-method-v2: a second input method on the same seat is told `unavailable`; the existing instance keeps working. |
| `9428a670` | Tablet tools report pressure 0.0 on proximity out. |
| `ef8cb061` | `wl_keyboard.repeat_info` is sent for inert keyboard objects. |
| `ee1a3346` | XWM holds a weak event-loop handle so `LoopInner` is not leaked (requires calloop 0.14.5). |
| `8a270232` | Keymap updates reach active keyboard grabs (input-method keyboard grabs). |
| `023b78b1` | `ForeignToplevelListState::remove_toplevel` removes the requested handle, not a live neighbour after a lazily dropped one. |

Deliberately not backported:

- The virtual-keyboard input-device rework (`7454d68a` and its follow-ups `553eaf9d`, `9786228e`) changes `VirtualKeyboardManagerState::new` and moves key delivery into the compositor's input pipeline. IMEs such as fcitx5 forward keys through a virtual keyboard, so adopting it needs a dedicated, hardware-validated change to JWM's keyboard path.
- `input_intercept` source/early-return (`254f8873`, `61eb2e00`), text-input activation hooks (`19c16d3e`), `PointerRc`/`TouchRc` and weak seat references (`39044641`, `d2079108`, `5c2ba413`, `0fb2a21d`): API changes with no JWM-visible fix.
- `wl_seat` `missing_capability` errors (`6bc53e3c`): spec-correct, but it disconnects clients that request a never-advertised device (for example touch on a laptop without a touchscreen). Revisit with the full upstream update.
- New protocols and backends not used by JWM (`2bf8be86` virtual pointer, `928d4a9b` winit gestures, `138ddbb6` ppc atomics).

## Local change

`src/input/touch/mod.rs`, `TouchInternal::cancel`. Cancellation creates a frame marker even after a previously framed batch, retires every active slot and deferred up-frame target, emits cancellation once per involved client target using the existing marker mechanism, and removes the marker. Previously a framed sequence had no pending marker, so cancellation returned without revoking it; unchanged slots and up-pending targets were also retained. Upstream (as of `24647fc8`) still has the original behavior.

This correction supports JWM's modal input isolation. It does not bypass authentication or change host input/security settings. The source-extracted old/new sequence tests are evidence about this state transition, not physical touchscreen or native compositor validation. JWM's permanent touch protocol regressions and full CI are required as integration gates.

Only this fixed base revision is patched via the root Cargo.toml Git-source patch. Do not change the version to suppress an advisory or replace it with an unrelated update. Review any future upstream update before removing or rebasing these changes: drop each backport that the new base already contains, and keep the local touch change until upstream fixes cancellation.

Permanent public-API regression suite: root `tests/smithay_touch_cancel.rs`, run by the ordinary `cargo test --locked --lib --bins --tests` gate when `wayland-backends` is enabled. It uses actual `TouchHandle`/`SeatState` with recording targets; no display socket, fake physical input, or native session is needed. Private marker/slot assertions are represented by no subsequent frame/motion/up delivery and a successful new sequence.
