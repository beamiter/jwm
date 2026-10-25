#!/usr/bin/env bash
# Foreground WaterLily worker and explicit IPC controls; no process-name kills.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/waterlily.sh [start [RUNNER_OPTIONS...] | status | stop | case [NAME] | palette [NAME]]

start    Enable the layer and run Julia in this terminal (default action).
         Reuse an already connected worker. Ctrl+C stops a newly started worker.
stop     Hide the layer only; stop the worker with Ctrl+C in its own terminal.
case     Select NAME, or cycle to the next case when omitted.
palette  Select NAME, or cycle to the next palette when omitted.
status   Print the WaterLily status JSON.

Runner options: --case, --device, --fps, --sim-size, --socket, --frame-file,
                --jelly-detail (each followed by a value).
Environment: JWM_TOOL, JULIA, JWM_WATERLILY_PROJECT override executable/project paths.
EOF
}

fail() { printf 'waterlily: %s\n' "$*" >&2; exit 1; }
action=${1:-start}
if (($#)); then shift; fi
case "$action" in
    -h|--help|help) usage; exit 0 ;;
    start)
        runner_args=("$@")
        while (($#)); do
            case "$1" in
                --case|--device|--fps|--sim-size|--socket|--frame-file|--jelly-detail)
                    if (($# < 2)) || [[ -z $2 || $2 == --* ]]; then
                        fail "$1 requires a value"
                    fi
                    shift 2 ;;
                *) fail "unknown runner option: $1 (see --help)" ;;
            esac
        done ;;
    case|palette)
        (($# <= 1)) || fail "$action accepts at most one name"
        name=${1:-next}
        [[ $name =~ ^[a-z][a-z0-9_]{0,31}$ ]] || fail "invalid $action name: $name" ;;
    status|stop) (($# == 0)) || fail "$action accepts no arguments" ;;
    *) fail "unknown action: $action (see --help)" ;;
esac

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
if [[ -z ${JWM_TOOL:-} ]]; then
    if command -v jwm-tool >/dev/null 2>&1; then
        JWM_TOOL=jwm-tool
    elif [[ -x $repo/target/release/jwm-tool ]]; then
        JWM_TOOL=$repo/target/release/jwm-tool
    elif [[ -x $repo/target/debug/jwm-tool ]]; then
        JWM_TOOL=$repo/target/debug/jwm-tool
    else
        fail 'jwm-tool not found; build it with cargo build --release --bin jwm-tool'
    fi
fi
command -v python3 >/dev/null 2>&1 || fail 'python3 is required to validate IPC status'

status=$("$JWM_TOOL" msg get_waterlily_status --raw) || fail 'status query failed; use a running composited X11 JWM session'
# Parse the actual success envelope, never a text match that could confuse an
# error or a nested extension field with the layer's enabled state.
flags=$(python3 -c '
import json, sys
try:
    reply = json.load(sys.stdin)
    if reply.get("success") is not True:
        raise ValueError(reply.get("error", "WaterLily is unavailable"))
    data = reply["data"]
    values = [data["enabled"], data["worker_connected"]]
    if any(type(value) is not bool for value in values):
        raise ValueError("invalid enabled/worker_connected fields")
    print(*(int(value) for value in values))
except (ValueError, KeyError, TypeError, AttributeError) as error:
    print(f"waterlily: invalid or unavailable IPC status: {error}", file=sys.stderr)
    sys.exit(1)
' <<< "$status") || exit 1
read -r enabled connected <<< "$flags"

case "$action" in
    status) printf '%s\n' "$status" ;;
    stop)
        if [[ $enabled == 1 ]]; then "$JWM_TOOL" msg toggle_waterlily; fi
        printf 'WaterLily layer hidden; Ctrl+C in the worker terminal stops the simulation.\n' ;;
    case|palette)
        [[ $connected == 1 ]] || fail 'no worker connected; run scripts/waterlily.sh start first'
        "$JWM_TOOL" msg "waterlily_$action" --args "\"$name\"" ;;
    start)
        if [[ $connected == 1 ]]; then
            ((${#runner_args[@]} == 0)) || fail 'a worker is already connected; use case/palette controls or stop it before changing runner options'
            if [[ $enabled == 0 ]]; then "$JWM_TOOL" msg toggle_waterlily; fi
            printf 'Reusing the connected WaterLily worker.\n'
            exit 0
        fi
        julia=${JULIA:-julia}
        command -v "$julia" >/dev/null 2>&1 || fail 'Julia not found; instantiate the waterlily project first'
        project=${JWM_WATERLILY_PROJECT:-$repo/waterlily}
        if [[ $enabled == 0 ]]; then "$JWM_TOOL" msg toggle_waterlily; fi
        exec "$julia" "--project=$project" --threads=auto "$repo/waterlily/runner.jl" \
            --device auto "${runner_args[@]}" ;;
esac
