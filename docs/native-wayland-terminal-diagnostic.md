# Native Wayland terminal-exit baseline (draft investigation)

This is a diagnostic-only branch based on `bffb50b0`. Do not merge the temporary
workflow into master. It is restricted to this draft branch and is not a new
required or permanent CI gate. Production compositor code is unchanged.

Run after building JWM with `backend-wayland-nested` and the
`crates/jwm_shell` example `native_shell_client`:

```sh
python3 scripts/test-terminal-exit-wayland.py --self-test
python3 scripts/test-terminal-exit-wayland.py --backend wayland-x11 --diagnose
python3 scripts/test-terminal-exit-wayland.py --backend wayland-winit --diagnose
```

`--diagnose` collects later cases after an invariant failure. It never changes
the final failure status or nonzero exit code. Keep the report, PNGs, native
client event receipts, config and session log together. Failure before the
clients map is a fixture/build/environment failure, not evidence of animation
behavior.

The clients are real native xdg-shell/wl_shm surfaces with DISPLAY removed.
An isolated Xvfb is only the nested compositor's output host and input injector.
Xwayland is installed because current nested startup requires that process; the
test clients do not use it. Native PID mapping and keyboard event receipts are
prerequisites, not inferred from X11 window IDs.

Covered diagnostics: ordinary floating, PiP and fullscreen overlap, early/mid/
late frames and cleanup, keyboard transfer during hide, pointer receipt after
hide, and interrupted reversal followed by keyboard receipt. The existing X11
coverage thresholds are reused without weakening that suite.

Not covered: native Above (no supported xdg-shell/IPC request), OR (not a native
Wayland concept), pointer transfer during animation, real lock credentials,
DRM/KMS, physical GPUs or native Wayland-hosted winit. A successful capture alone
is not a full behavioral pass. No native Wayland pass has yet been established.
