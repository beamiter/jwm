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

## Run the daily-drive acceptance gate

```sh
python3 scripts/acceptance_evidence.py daily                         # checklist, exit 1
python3 scripts/acceptance_evidence.py daily /path/to/candidate/*.json
```

This read-only check does not start a compositor, log in, exercise a portal,
or change a desktop. It prints separate `isolated_ready` (nested compositor
and private host/portal cases) and `physical_ready` (native Wayland/KMS cases)
results. The physical track has eight cases, including a distinct mixed-scale
case. Exit 0 means both tracks passed for the supplied evidence. A machine
without the second physical output cannot pass the dual-output cases; a CI,
Xvfb or nested run does not stand in for those or any other physical case.

Use the generated checklist to select a case and run it in an owned session
that can be discarded. For native login, rollback, hotplug, suspend, IME and
portal, retain the observations and actual binary/config identities from that
run. Do not use the working desktop for the disruptive cases. See
[hardware validation](hardware-validation.md) for the broader release matrix
and [virtual keyboard](virtual-keyboard.md#real-desktop-gate-not-completed-in-the-build-environment)
for the physical IME scenarios. The daily-drive check does not replace the
separate release gate or validate the truth of operator-supplied logs.

Record completed attempts with the existing command, using only real paths
from that run. For example, after observing an isolated compositor pass:

```sh
python3 scripts/acceptance_evidence.py record \
  --case compositor-isolated --level isolated --backend wayland-nested \
  --status pass --binary /path/to/tested/jwm --config /path/to/tested/config.toml \
  --evidence /path/to/runner.log --dependency src \
  --dependency Cargo.toml --dependency Cargo.lock \
  --output /path/to/candidate/compositor-isolated.json
```

Use `--backend host` for the private session-host case and `wayland-udev`
for physical cases. Passes require the tested config in addition to the
binary, source fingerprints and raw observations. Where applicable, include
each actual helper/previous binary (for example in rollback) and add source
dependencies for the runner and tested helpers. A physical record needs
`--hardware` with the actual GPU vendor, driver, explicit
`"software_renderer": false`, and an `outputs` list with distinct `name` and
`mode` fields. For the two-output cases record the initial two-output
topology, and retain before/after observations of the transition in the log.
Record `fail` or `skip` with a reason instead of supplying placeholder logs.
Keep prior candidates' attempts separately: a failure or stale pass supplied
at the required scope remains a blocker even if another supplied run passes.
Report files can contain private local paths or hardware details; review
before sharing.

### Mixed-scale pointer and visual check

On two physical outputs in an owned native Wayland/KMS session, collect a fresh
`zwlr_output_manager_v1` completed state with the **effective** announced scales,
not values read from the config. Select two active connectors with distinct
positive finite scales. Map a contrasting native pointer-sensitive visual target
on each output. Cross from one target to the other with the local pointer:
retain both actual `wl_pointer.enter` receipts and separate visual captures.
Record all coordinates in common compositor-logical units. Convert local
pointer coordinates using the fixture target origin, and independently measure
the visible target bounds in the captured image in the same units. Use observed
output positions, modes, transforms and scales to derive logical output
rectangles, or read matching observed logical geometry; do not substitute
configured scale or guess when a connector cannot be identified.

Set `hardware.mixed_scale_trace` to the trace's absolute path. Include the
trace plus each of the two visual captures in that record's `--evidence`
arguments so their hashes are checked; attach raw protocol and input logs too.
Keep raw evidence outside the directory glob used for candidate records. Trace
shape (illustrative numbers and paths only, not valid acceptance evidence):

```json
{
  "format": "jwm-mixed-scale-v1",
  "protocol": "zwlr_output_manager_v1",
  "heads": [
    {"name": "DP-1", "effective_scale": 1, "logical_rect": [-100, 0, 100, 100]},
    {"name": "HDMI-A-1", "effective_scale": 1.5, "logical_rect": [0, 0, 100, 100]}
  ],
  "pointer_visual_samples": [
    {"output": "DP-1", "input_event": "wl_pointer.enter", "pointer_logical": [-10, 30],
     "visual_target_logical_rect": [-20, 20, 20, 20], "visual_evidence": "/raw/before.png"},
    {"output": "HDMI-A-1", "input_event": "wl_pointer.enter", "pointer_logical": [10, 30],
     "visual_target_logical_rect": [0, 20, 20, 20], "visual_evidence": "/raw/after.png"}
  ]
}
```

Rectangles are `[x, y, width, height]`; points are `[x, y]`. The gate rejects
equal or non-finite scales, wrong or duplicate output names, pointer coordinates
outside the announced head or observed visual target, and missing, altered or
byte-identical visual artifacts, even under different names. Distinct file
hashes only rule out identical bytes: the gate does not inspect image pixels,
verify capture timestamps or confirm the operator actually ran the interaction.
Same-scale, headless, nested and static-config checks do not pass. Report `skip` with an explicit
reason if the required setup or interactive observation is unavailable.

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

`jwm-tool health` prints compact `idle` for daily drive operators.
