#!/usr/bin/env bash
# Offline checks: run the just-built binary and retain private, collision-free logs.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-runtime-test.XXXXXXXX")
TAG=${TEST_ROOT##*/}
cleanup() {
    rm -f -- "/tmp/jwm_debug_$TAG.log" "/tmp/jwm_winit_$TAG.log"
    rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT
REPO="$TEST_ROOT/project space"
mkdir -p "$REPO/scripts" "$TEST_ROOT/bin"
cp "$ROOT/scripts/run_nested.sh" "$ROOT/scripts/debug_jwm.sh" "$REPO/scripts/"
cat > "$TEST_ROOT/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" > "$CASE_DIR/cargo.log"
target=${CARGO_TARGET_DIR:-target} profile=debug
while (($#)); do
    case "$1" in
        --target-dir) target=$2; shift 2 ;;
        --release) profile=release; shift ;;
        *) shift ;;
    esac
done
mkdir -p "$target/$profile"
printf '#!/usr/bin/env bash\nprintf "NEW_BINARY_EXECUTED\\n"\nexit "${JWM_TEST_STATUS:-0}"\n' > "$target/$profile/jwm"
chmod +x "$target/$profile/jwm"
STUB
cat > "$TEST_ROOT/bin/jwm" <<'STUB'
#!/usr/bin/env bash
printf 'DEBUG_LOG_CONTENT\n'
exit "${JWM_TEST_STATUS:-0}"
STUB
cat > "$TEST_ROOT/bin/date" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$TAG"
STUB
chmod +x "$TEST_ROOT/bin/"*
export TAG PATH="$TEST_ROOT/bin:$PATH"
fail() { printf 'test-runtime-helpers: FAIL: %s\n' "$*" >&2; exit 1; }
check_log() {
    local log=$1
    [[ $log == "$CASE_DIR/"* && -f $log && ! -L $log ]] || fail "log not privately allocated under TMPDIR: $log"
    [[ $(stat -c %a "$log") == 600 ]] || fail 'log permissions are not 0600'
}
run_nested() {
    local profile=$1 target=$2 status=0
    export CASE_DIR="$TEST_ROOT/nested-$profile-${3:-success}"
    mkdir -p "$CASE_DIR" "$REPO/target/$profile"
    printf '#!/bin/sh\necho OLD_BINARY_EXECUTED\n' > "$REPO/target/$profile/jwm"
    chmod +x "$REPO/target/$profile/jwm"
    TMPDIR="$CASE_DIR" DISPLAY=:fixture CARGO_TARGET_DIR="$target" JWM_TEST_STATUS="${4:-0}" \
        bash "$REPO/scripts/run_nested.sh" winit "$profile" > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == "${4:-0}" ]] || fail "nested status $status, expected ${4:-0}"
    grep -q NEW_BINARY_EXECUTED "$CASE_DIR/output.log" || fail 'nested did not run the newly built binary'
    ! grep -q OLD_BINARY_EXECUTED "$CASE_DIR/output.log" || fail 'nested ran the stale default-target binary'
    check_log "$(sed -n 's/^   日志://p' "$CASE_DIR/output.log")"
}
printf 'preserve debug\n' > "$TEST_ROOT/debug-sentinel"
printf 'preserve nested\n' > "$TEST_ROOT/nested-sentinel"
ln -s "$TEST_ROOT/debug-sentinel" "/tmp/jwm_debug_$TAG.log"
ln -s "$TEST_ROOT/nested-sentinel" "/tmp/jwm_winit_$TAG.log"
run_nested debug 'relative target'
run_nested release "$TEST_ROOT/absolute target"
run_nested debug 'relative target' failure 19
[[ $(cat "$TEST_ROOT/nested-sentinel") == 'preserve nested' ]] || fail 'nested followed a predictable log symlink'
for expected in 0 19; do
    export CASE_DIR="$TEST_ROOT/debug-$expected"
    mkdir -p "$CASE_DIR"
    status=0
    TMPDIR="$CASE_DIR" JWM_TEST_STATUS="$expected" bash "$REPO/scripts/debug_jwm.sh" > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == "$expected" ]] || fail "debug status $status, expected $expected"
    check_log "$(sed -n 's/^📝 Logging to: //p' "$CASE_DIR/output.log")"
    if ((expected != 0)); then
        ! grep -q '✅' "$CASE_DIR/output.log" || fail 'debug reported success for failed jwm'
    fi
done
[[ $(cat "$TEST_ROOT/debug-sentinel") == 'preserve debug' ]] || fail 'debug followed a predictable log symlink'
printf 'test-runtime-helpers: PASS\n'
