# Daily-drive task loops

SOTA polish for JWM is measured by **closed daily tasks**, not by binding
count. Each loop below should be completable without reading the source tree.

## Shell loops

| Task | Success | Max actions | Notes |
| --- | --- | --- | --- |
| Switch default audio output after plugging headphones | Named OSD confirms; Hub or picker shows new default | ≤2 | Prefer Control Center Volume / device picker |
| Mute mic | OSD + Hub Input state agree | 1 | Middle-click or `m` on Input row |
| Copy screenshot to clipboard and paste in a client | Native offer (no required `wl-copy`) | 2 | Alt+S → editor → Ctrl+C, or Space/Enter save |
| Lock and unlock | Lock covers outputs; password/ PAM path works | 2 | Idle lock also acceptable |
| Launch an app | App focused on current tags | 2 | Alt+R launcher |
| Restore after crash/restart | Tags / floating geometry return | 1 | Session restore |

## Effects honesty

| Rule | Why |
| --- | --- |
| Effects must be toggleable without restart | Battery and game fullscreen |
| Direct-scanout / VRR diagnostics stay truthful | No fake "active" |
| Damage-driven idle when nothing moves | Control-plane metrics must show it |
| Cube/prism/wobbly never block lock or quit | Safety over spectacle |

## Control plane (selling point)

These are first-class product features, not maintainer-only tools:

```bash
jwm --backend wayland-udev --doctor          # offline gate
jwm-tool health --json                       # live snapshot
jwm-tool capabilities --json                 # discover IPC safely
jwm-support --backend wayland-udev --output bundle.json
jwm-tool perf record --out baseline.json     # labeled only
```

A build that cannot explain what it supports is not daily-drive ready.
