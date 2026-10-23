# Reproducible compositor effect matrix

`scripts/compositor-effect-matrix.py` wraps the existing `jwm-tool perf record`
and `perf compare`; it does not introduce another timing engine. Python 3.11+
is required (Python 3.10 can use `tomli`). Generate 144 explicit cases across 1080p, 4K, two 1080p monitors,
six effect combinations and idle/animation/tag-switch/window-move workloads:

```sh
python3 scripts/compositor-effect-matrix.py plan --out /tmp/matrix-plan.json
python3 scripts/compositor-effect-matrix.py prepare \
  --case 1080p-all-tag-switch-cube --template /path/to/base-config.toml \
  --wallpaper /path/to/static.jpg --earth-texture /path/to/earth.jpg \
  --out /path/to/dedicated-session/config/jwm/config_x11.toml
```

Use a dedicated session and the prepared configuration. Start a visible
`xmessage` fixture for the default glass rule (`1:Xmessage`); `--window-class`
can select another fixture. Preserve the same windows, topology, refresh rates,
driver and assets between revisions. Confirm the celestial image loaded;
Astronomy is currently X11-only and the recorder refuses its Wayland fallback.
The animation workload opens/closes overview every 500ms; tag switching uses
tags 1 and 2 every 500ms; window movement uses a 30Hz stepped path in the float
layout. These are explicitly different workloads, not interchangeable samples.

```sh
python3 scripts/compositor-effect-matrix.py record \
  --case 1080p-all-tag-switch-cube --config /path/to/dedicated-session/config/jwm/config_x11.toml \
  --tool /path/to/jwm-tool --revision COMMIT_SHA --out /tmp/matrix/run-A \
  --workload-json '["python3","scripts/compositor-matrix-workload.py","tag-switch","--tool","/path/to/jwm-tool"]'
python3 scripts/compositor-effect-matrix.py summary /tmp/matrix --out /tmp/matrix.csv
python3 scripts/compositor-effect-matrix.py compare /tmp/matrix/run-A /tmp/matrix/run-B
```

Idle cases omit `--workload-json`. Non-idle cases require it and fail if the
fixture dies early. Recording stores the exact TOML, live configuration hash,
topology, source revision and command alongside the raw performance report.
It refuses mismatched settings/topology, a different config file, or changes
during measurement. Comparisons require the same case, live config, topology
and workload before invoking the project's existing regression budgets.
An incomplete run cannot pass that gate. Use at least three repeated recordings
per cell after shader/asset warmup before drawing optimization conclusions.

The CSV includes CPU, memory, mean/p95/p99 frame time and damage fraction.
**GPU timer measurements are unavailable in the existing perf tool** and stay
empty, with an explicit reason in metadata. Do not substitute its CPU-side
zone durations or placeholder GPU load fields for GPU time. Attach vendor or
timer-query traces on real hardware separately. Software Mesa runs are labeled
as such, and do not validate hardware performance, direct scanout or physical
multi-monitor behavior.

Run the offline tooling regressions with:

```sh
python3 scripts/test-compositor-effect-matrix.py
```
