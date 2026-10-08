# X11 backend parity

JWM has two native X11 transports: `x11rb` (`backend-x11rb`) and `xcb`
(`backend-xcb`). `src/backend/x11` is their shared window-management and
compositor implementation, not a third selectable backend. Both transports
must expose the same observable behavior through `backend::api`.

## Contract and ownership

| Surface | Shared contract / implementation | Transport-specific coverage |
| --- | --- | --- |
| Window lifecycle, configure, stacking, EWMH | `x11/wm`, backend API | Existing lifecycle, event-coalescing, property and method-parity tests |
| WM startup ownership | Root `SubstructureRedirect` must be accepted before publishing WM properties | X11RB now checks exclusive event-mask claims synchronously, matching XCB |
| Pointer grabs and capture endpoints | Continuous motion, button press/release coordinates, shared capture policy | The X11RB capture-motion regression now runs automatically on a private Xvfb server |
| Pointer warping | Root coordinates must actually move the pointer when the capability is advertised | Real-server root-warp round trips for both transports; X11RB no longer inherits a no-op |
| Compositor, effects, recording and screenshots | Shared X11 compositor and capability delegation | Shared implementation and existing parity guards; GPU/driver behavior still needs hardware validation |
| Toast input regions | Replace the overlay INPUT shape, restore click-through on dismissal | Both XCB adapter layers implement the required hook; real-server replacement, clearing, failure and retry checks run for both transports |
| Clipboard | Text/PNG, TARGETS, MULTIPLE, TIMESTAMP, INCR, SAVE_TARGETS | XCB rejects empty/multiple-value INCR size headers without indexing an empty list; malformed-transfer drain/recovery regression |
| Outputs and gamma | RandR monitor and CRTC fallback IDs preserve connector metadata and target the same CRTC | X11RB fallback preserves EDID/HDR metadata and accepts CRTC IDs for gamma; real-server property/gamma fixtures |
| Cursors, keys, window properties | Shared semantic helpers and complete backend API overrides | Bidirectional method-parity guard for WindowOps, InputOps, PropertyOps, OutputOps, KeyOps, EWMH, colors and cursors |

The compositor's input-shape hook is mandatory, so a future transport cannot
silently inherit a successful no-op. Both implementations destroy temporary
XFixes regions even when applying the shape fails. They report server rejection
so the compositor does not cache an unapplied shape as successful.

Protocol mechanics need not be identical. XCB uses native checked requests;
X11RB can queue ordinary updates and reports asynchronous errors through its
event loop. Exclusive WM ownership and shape-cache success are correctness
boundaries where the server result must be observed. Inactive tray-host code
and the independent X11RB remote helper are not missing XCB WM features.

## Validation

Install the dependencies documented in `CONTRIBUTING.md`, including Xvfb. The
native tests launch private servers using the existing `IsolatedXvfb` fixture;
they never inject input into the active desktop and do not require `DISPLAY`.

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
EGL_PLATFORM=surfaceless JWM_REQUIRE_HEADLESS_GL=1 scripts/test.sh --lib --bins --tests

for backend in backend-x11rb backend-xcb; do
  RUSTFLAGS='-D warnings' cargo check --locked --all-targets \
    --no-default-features --features "$backend"
  scripts/test.sh --no-default-features --features "$backend" --lib --bins --tests
done
```

The test wrapper supplies `--locked` itself. A sandbox must permit private Unix
sockets and child-process cleanup for Xvfb and the process-boundary tests.

Xvfb protocol round trips and surfaceless software rendering do not establish
physical-monitor HDR/gamma behavior, driver-specific GLX/EGL presentation,
interactive toast rendering, capture/video quality, or real Wayland session
behavior. Those require the hardware/session checks in
[`hardware-validation.md`](hardware-validation.md).
