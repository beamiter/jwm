# Wayland virtual-keyboard migration

This is an isolated compatibility migration of Smithay's virtual-keyboard
input-device API. It does not claim that existing Chinese input was broken,
nor that protocol tests validate a real fcitx5 desktop.

## Event paths

| Source | Path | Policy |
| --- | --- | --- |
| Physical keyboard | udev / X11-nested / winit → existing lock/layer/modal/shortcut filter → Smithay keyboard → IME grab or focused client | Unchanged |
| Virtual keyboard | zwp_virtual_keyboard_v1 → Smithay device event → JwmWaylandState callback → compatibility delivery → focused wl_keyboard | Per-device map/modifiers; no IME re-entry or WM shortcut emission |
| IME composition | text-input-v3 enable/commit → input-method-v2 preedit/commit_string/commit → active text input | Unchanged; covered by wire tests |
| Focus | SeatHandler::focus_changed → invalidate accepted virtual key presses → normal data-device focus | Prevents old releases after A→B→A |
| Session lock | session_locked + lock epoch → drop virtual keys/modifiers, clear transient device state | Physical input continues through the existing locker route |
| JWM modal UI | virtual_input_blocked (lock card, shell panels, screenshot/long-screenshot capture, inactive session) → release held keys to their target, drop the rest | Focus does not move, so held keys are released explicitly |
| Monitor-lock shade | focused window shown only behind shades → same as modal UI | Follows keyboard focus, not the pointer |

The upstream commits are `7454d68a`, `553eaf9d`, and `9786228e`. Their source
changes are backported together. Upstream's example installs a virtual keymap
into the physical seat and explicitly does not restore it; that example is not
suitable for JWM's compatibility requirements.

The local Smithay `forward_virtual_keyboard` bridge retains the old independent
virtual XKB state. It uses Smithay's keymap cache and focused wl_keyboard
selection. The bridge does not modify physical pressed keys, modifier locks,
keymap, LEDs or IME grab state. This also avoids feeding IME-forwarded keys
back into that IME, without trying to infer IME identity from popup creation.

Virtual input does **not** gain new global-shortcut behavior in this change.
Standalone virtual keyboards retain the old pass-through policy. Generalizing
those devices to the physical shortcut pipeline needs a separate source-aware
policy change and desktop validation. The new upstream APIs are available for
that future step. Virtual key presses do not become trusted popup/activation
serials, nor do they enter the WM backend-event queue.

Destroy/disconnect releases only accepted keys still belonging to the same
focus and lock generation. No release is sent to a newly focused client.
No-keymap devices follow configured seat keymap reloads. Modifier masks can be
stored while no client is focused but are never applied to physical XKB state.
All virtual input is blocked during a session lock, including virtual input
into the locker; enabling an on-screen keyboard for a locker would require an
explicit authenticated policy, outside this change. JWM's own lock card, its
shell panels and interactive captures block virtual input the same way while
they keep physical keys from clients; keyboard focus stays on the app then, so
a key held when the block began is released to that app instead of repeating.
A monitor-lock shade blocks virtual input only for a focused window shown
behind it: typing continues on the unlocked monitor wherever the pointer rests.
Keymaps are read with a bounded `pread`, never memory-mapped, so a size larger
than the client's file cannot crash the compositor.

## Reproducible automated checks

On a Linux machine with the repository's CI build dependencies:

```sh
scripts/test.sh --lib --no-default-features --features backend-wayland-nested virtual_keyboard_regressions
scripts/test.sh --lib --bins --tests --no-default-features --features backend-wayland-nested
EGL_PLATFORM=surfaceless JWM_REQUIRE_HEADLESS_GL=1 scripts/test.sh
cargo check --locked --all-targets
cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
cargo fmt --all -- --check
```

The socket tests use the production `JwmWaylandState`, real registry binds,
SCM_RIGHTS keymaps, virtual-keyboard requests, keyboard events, input-method
keyboard grabs and text-input-v3 requests. UTF-8 preedit/commit assertions test
transport of an IME-selected candidate; they do not exercise candidate search,
rendering, click selection or fcitx's dictionary.

## Real desktop gate (not completed in the build environment)

Record JWM commit, backend, fcitx5 version/addons, keyboard layouts, toolkit
versions and pass/fail evidence. Use a disposable nested session first, then
DRM/KMS. Do not replace a working desktop until this gate passes.

| Scenario | Required observation |
| --- | --- |
| GTK and Qt Wayland text fields; browser and terminal | Type `nihao`, see composition/candidates, confirm `你好` with Space/number/Enter exactly once |
| Chinese/English toggle, punctuation, Backspace, Escape | No missing/duplicate keys; cancellation and punctuation are correct |
| Unhandled fcitx keys | Reach the app once; no CPU spin or keyboard-grab loop |
| Physical Shift/Ctrl/Alt held while enabling/disabling IME | No stuck or lost modifier; CapsLock/NumLock survive focus and IME destruction |
| Two windows, pointer focus, Alt+Tab, tag switch, popup | Composition follows/deactivates on focus correctly; old releases do not reach the new window |
| Configured global shortcuts and shortcut-inhibited app | Physical bindings retain their prior behavior; virtual pass-through does not add bindings |
| Different virtual and physical keymaps; reload layout | First subsequent physical key uses the configured physical map |
| Lock before/during composition; unlock; kill/restart fcitx | No app input while locked, no virtual locker input, no replay after unlock; physical locker input works |
| Candidate popup and click selection | Correct position and selected text under multi-output scale changes |

## Rollback

Keep the upstream backport, compositor adapter, tests and this documentation in
one atomic commit. Revert that commit and rebuild to recover the previous
virtual-keyboard implementation. No configuration or saved-state migration is
required. Do not revert only the compositor adapter: the upstream API requires
its callback implementation.
