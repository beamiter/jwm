# WaterLily provider migration

The Julia implementation now lives in
[beamiter/juliafun/waterlily](https://github.com/beamiter/juliafun/tree/master/waterlily).
JWM no longer contains the WaterLily simulation, case registry, Julia project,
manifest, or provider tests. The package name/UUID remain `JwmWaterLily` for
compatibility. The detailed case, physics, GPU and worker protocol documentation
moved with the implementation to that directory's `README.md`.

## Existing effects continue to work

The original X11RB/XCB effect consumer is retained as a compatibility adapter;
its shaders, volume/material renderer and negotiated `jelly-pose-v1` rendering
have not been replaced by a generic flat image. Existing `JWMLILY` protocol
versions 1–4, config keys `waterlily_enabled` / `waterlily_opacity`, and all old
`JWM_WATERLILY_*` environment variables continue to work.

```bash
export JWM_WATERLILY_PROJECT=/path/to/juliafun/waterlily
julia --project="$JWM_WATERLILY_PROJECT" -e 'using Pkg; Pkg.instantiate()'
scripts/waterlily.sh start --case wander --device cpu
scripts/waterlily.sh case dance
scripts/waterlily.sh palette fluent
scripts/waterlily.sh status
scripts/waterlily.sh stop
```

The helper discovers `../juliafun/waterlily` for sibling checkouts. Otherwise set
`JWM_WATERLILY_PROJECT`; both the Julia environment **and runner** are loaded
from that location. Missing providers are rejected before enabling an effect.
`JWM_TOOL` and `JULIA` still select executable paths. No provider is installed,
spawned or granted new permissions automatically by JWM.

`stop` hides the layer; Ctrl+C in the worker's own terminal stops its process.
The helper reuses an already-connected worker only when no new runner options
are supplied. Case/palette hot-switches still use these IPC actions:

```bash
jwm-tool msg toggle_waterlily
jwm-tool msg waterlily_case --args '"dance"'
jwm-tool msg waterlily_palette --args '"fluent"'
jwm-tool msg get_waterlily_status --raw
```

`get_waterlily` / `get_wly` aliases and compact `jwm-tool health` output remain
unchanged. The older `toggle_excalibur` alias remains deprecated. The legacy
adapter's pointer behavior is unchanged; it is **not** a capability of the new
protocol. The legacy layer remains unavailable on Wayland as before.

## Generic desktop visual extensions

See [Desktop visual protocol](desktop-visual.md) for the independent rendering
channel and SDK/example. Any program can publish ordinary RGBA without linking
Julia or WaterLily, loading arbitrary compositor code, or obtaining screen/input
access. The migrated WaterLily worker offers `--transport desktop-visual` as an
explicit opt-in. This mode projects 3D cases to ordinary planar pixels and does
not carry legacy refraction fields, jelly poses, or pointer commands. `rain`
and `puddle` require the legacy adapter and are rejected in generic mode.

Generic and legacy transports are intentionally separate: old effect switches
never toggle an unrelated generic producer or silently grant it input access.
The legacy socket and shared-file paths still default to
`$XDG_RUNTIME_DIR/jwm-waterlily.sock` and `jwm-waterlily.frame`, respectively.

## Validation ownership and licensing

JWM keeps Rust reader/renderer tests and shader validation. Julia provider and
SDK tests run in juliafun. Its `waterlily/THIRD_PARTY_NOTICES.md` records inherited
source attribution and a jelly-example provenance review still needed before
redistribution; extracting the files is not a relicensing decision.
