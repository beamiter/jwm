#!/usr/bin/env bash
# Run the jwm test suite without leaking child processes.
#
# Several tests spawn real subprocesses (stubborn shells, `sleep`, `cat`,
# fake ffmpeg) to exercise terminate/reap logic. If the harness is killed
# half-way, or if it runs inside a sandbox that denies `kill()`, those
# children can outlive the run and pin a CPU core. This wrapper:
#
#   1. refuses to run inside a sandbox that blocks signals (the tests would
#      fail for environmental reasons and the agent must not "fix" the code);
#   2. runs cargo in its own process group and SIGKILLs the whole group on
#      exit, whatever the reason;
#   3. sweeps for known leaked test children afterwards.
#
# Usage: scripts/test.sh [extra cargo test args...]
#   e.g. scripts/test.sh --lib
#        scripts/test.sh --lib -- monitor_management
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

# --- 1. preflight: can this environment deliver signals? -------------------
sleep 30 &
probe=$!
if ! kill -KILL "$probe" 2>/dev/null; then
    cat >&2 <<'MSG'
scripts/test.sh: this shell cannot send signals to its own children
(kill() returned EPERM). You are running inside a sandbox such as the
Cursor agent terminal sandbox. Process-management tests WILL fail here for
environmental reasons only. Do not modify source code in response; rerun
the tests outside the sandbox instead.
MSG
    wait "$probe" 2>/dev/null
    exit 2
fi
wait "$probe" 2>/dev/null

# --- 2. run cargo in its own process group, kill the group on exit ---------
if [[ "${#}" -eq 0 ]]; then
    set -- --locked --lib --bins --tests
else
    set -- --locked "$@"
fi

setsid cargo test "$@" &
cargo_pid=$!

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    # Kill everything in cargo's process group (negative pid), then reap.
    kill -KILL -- "-$cargo_pid" 2>/dev/null
    wait "$cargo_pid" 2>/dev/null
    # --- 3. sweep known leaked test children --------------------------------
    local leaked
    leaked=$(pgrep -f "trap '' TERM" 2>/dev/null || true)
    if [[ -n "$leaked" ]]; then
        echo "scripts/test.sh: killing leaked test children: $leaked" >&2
        # shellcheck disable=SC2086
        kill -KILL $leaked 2>/dev/null
    fi
    exit "$status"
}
trap 'cleanup' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

wait "$cargo_pid"
