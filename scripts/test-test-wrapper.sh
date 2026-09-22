#!/usr/bin/env bash
# Offline process-boundary regression tests for scripts/test.sh.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
PROJECT_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd -P)
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-test-wrapper.XXXXXXXX")
FAKE_BIN="$TMP_ROOT/bin"
STATE_DIR="$TMP_ROOT/state"
outsider_pid=
wrapper_pid=

cleanup() {
    trap - EXIT HUP INT TERM
    if [[ -n $wrapper_pid ]]; then
        kill -TERM "$wrapper_pid" 2>/dev/null || true
        wait "$wrapper_pid" 2>/dev/null || true
    fi
    if [[ -n $outsider_pid ]]; then
        kill -KILL -- "-$outsider_pid" 2>/dev/null || true
        wait "$outsider_pid" 2>/dev/null || true
    fi
    case $TMP_ROOT in
        "${TMPDIR:-/tmp}"/jwm-test-wrapper.*) rm -rf -- "$TMP_ROOT" ;;
        *) printf 'Refusing to clean unexpected test path: %s\n' "$TMP_ROOT" >&2 ;;
    esac
}
trap cleanup EXIT HUP INT TERM

fail() {
    printf 'test-test-wrapper: FAIL: %s\n' "$*" >&2
    exit 1
}

process_is_live() {
    local pid=$1 state
    kill -0 "$pid" 2>/dev/null || return 1
    state=$(ps -o stat= -p "$pid" 2>/dev/null) || return 1
    [[ $state != Z* ]]
}

wait_for_file() {
    local path=$1 attempt
    for ((attempt = 0; attempt < 100; attempt++)); do
        [[ -s $path ]] && return 0
        sleep 0.02
    done
    fail "timed out waiting for $path"
}

wait_for_exit() {
    local pid=$1 attempt
    for ((attempt = 0; attempt < 100; attempt++)); do
        process_is_live "$pid" || return 0
        sleep 0.02
    done
    fail "process $pid was not cleaned up"
}

mkdir -p -- "$FAKE_BIN" "$STATE_DIR"

# This cargo stand-in records both the process-group leader and a child that
# ignores TERM. With TEST_CARGO_STATUS unset it waits forever; with the
# variable set it exits immediately and deliberately leaves the child behind.
# shellcheck disable=SC2016 # The generated script expands these variables.
printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -eu' \
    'printf "%s\n" "$$" > "$TEST_STATE_DIR/cargo.pid"' \
    'bash -c '\''trap "" TERM; while :; do sleep 1; done'\'' &' \
    'child=$!' \
    'printf "%s\n" "$child" > "$TEST_STATE_DIR/child.pid"' \
    'if [[ -n ${TEST_CARGO_STATUS:-} ]]; then exit "$TEST_CARGO_STATUS"; fi' \
    'wait "$child"' > "$FAKE_BIN/cargo"
chmod 0755 -- "$FAKE_BIN/cargo"

# A process with the signature targeted by the old global pgrep sweep lives
# outside the wrapper's test process group and must remain untouched. Give it
# a separate group so test teardown also removes its current sleep child.
setsid bash -c "trap '' TERM; while :; do sleep 1; done" &
outsider_pid=$!

INTERRUPT_STATE="$STATE_DIR/interrupt"
mkdir -p -- "$INTERRUPT_STATE"
PATH="$FAKE_BIN:$PATH" TEST_STATE_DIR="$INTERRUPT_STATE" \
    bash "$PROJECT_ROOT/scripts/test.sh" --lib > "$TMP_ROOT/wrapper.log" 2>&1 &
wrapper_pid=$!

wait_for_file "$INTERRUPT_STATE/cargo.pid"
wait_for_file "$INTERRUPT_STATE/child.pid"
cargo_pid=$(<"$INTERRUPT_STATE/cargo.pid")
child_pid=$(<"$INTERRUPT_STATE/child.pid")

cargo_pgid=$(ps -o pgid= -p "$cargo_pid") || fail "cargo stand-in exited early"
child_pgid=$(ps -o pgid= -p "$child_pid") || fail "cargo child exited early"
cargo_pgid=${cargo_pgid//[[:space:]]/}
child_pgid=${child_pgid//[[:space:]]/}
[[ $cargo_pgid == "$cargo_pid" ]] || fail "cargo did not lead its isolated process group"
[[ $child_pgid == "$cargo_pgid" ]] || fail "cargo child escaped the isolated process group"

kill -TERM "$wrapper_pid"
set +e
wait "$wrapper_pid"
wrapper_status=$?
set -e
wrapper_pid=

[[ $wrapper_status -eq 143 ]] || fail "TERM produced status $wrapper_status instead of 143"
wait_for_exit "$cargo_pid"
wait_for_exit "$child_pid"
process_is_live "$outsider_pid" || fail "cleanup killed an unrelated matching process"

run_completion_case() {
    local name=$1 cargo_status=$2 expected_status=$3
    local case_state="$STATE_DIR/$name" cargo_pid child_pid wrapper_status
    mkdir -p -- "$case_state"

    PATH="$FAKE_BIN:$PATH" TEST_STATE_DIR="$case_state" \
        TEST_CARGO_STATUS="$cargo_status" \
        bash "$PROJECT_ROOT/scripts/test.sh" --lib \
        > "$TMP_ROOT/$name.log" 2>&1 &
    wrapper_pid=$!

    set +e
    wait "$wrapper_pid"
    wrapper_status=$?
    set -e
    wrapper_pid=

    [[ $wrapper_status -eq $expected_status ]] ||
        fail "$name produced status $wrapper_status instead of $expected_status"
    wait_for_file "$case_state/cargo.pid"
    wait_for_file "$case_state/child.pid"
    cargo_pid=$(<"$case_state/cargo.pid")
    child_pid=$(<"$case_state/child.pid")
    wait_for_exit "$cargo_pid"
    wait_for_exit "$child_pid"
    process_is_live "$outsider_pid" ||
        fail "$name cleanup killed an unrelated matching process"
}

run_completion_case success 0 0
run_completion_case failure 42 42

printf 'test-test-wrapper: PASS\n'
