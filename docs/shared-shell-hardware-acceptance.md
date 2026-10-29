# Native output acceptance

The native layer-shell implementation has passed nested Wayland tests with the
production Leptos header. Physical DRM/KMS acceptance is still open. The current
machine has one connected eDP output, an active X11 session on seat0/VT2, and no
connected external display. The [current preflight record](../artifacts/header-alignment/hardware-preflight-current.json)
records connector modes and the active X11 session without changing them; the
[original device preflight](../artifacts/shared-shell-wayland/hardware-preflight.json)
is retained separately.

Use a separate login session and preserve the current desktop before running the
physical tests. Run the installed `jwm --backend wayland-udev` as the logged-in
user, with `status_bar.name = "tauri_leptos_bar"` and an explicit enabled
`[desktop_shell]` table in a backed-up Wayland configuration. Do not infer any of
the following results from Xvfb, logical RandR monitors, or an outer nested
window's DPI.

| Physical test | Required observations | Current result |
|---|---|---|
| DRM/KMS startup | One host; three native registered panels per output; actual compositor capture; no orphan reservations after exit | Not run |
| Fractional output scale | Actual inner output scale changes; correctly sized input regions, preview anchors and reservations | Not run |
| Mixed clients | Native and XWayland focus, minimize/restore, fullscreen, preview and header replacement | Nested pass; physical run pending |
| Multiple outputs | External display connected; negative positions; per-output identities and one panel set each | No external display connected |
| Physical hotplug | Unplug/replug cable; old preview/action identities rejected; surviving output remains usable | Not run |
| Session recovery | Host/top crash and JWM restart retain applications; disable removes all surfaces and reservations | Nested pass; physical run pending |

Archive the actual `get_desktop_shell`, `get_windows` and output diagnostics,
compositor screenshots, executable SHA256 values and logs for each test. Record
the tested scale and connectors. Restore the previous configuration and return
to the original session after the run. P6 remains open until these observations
are available.
