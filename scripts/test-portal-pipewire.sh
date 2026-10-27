#!/usr/bin/env bash
# Exercise native stream startup/shutdown against an isolated PipeWire daemon.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
PREFIX=${JWM_PIPEWIRE_PREFIX:-"$HOME/.cache/jwm/pipewire-prefix"}
PIPEWIRE_BIN=${JWM_PORTAL_TEST_PIPEWIRE_BIN:-"$PREFIX/bin/pipewire"}
if [[ ! -x $PIPEWIRE_BIN ]]; then
    echo 'test-portal-pipewire: set JWM_PORTAL_TEST_PIPEWIRE_BIN to a PipeWire daemon' >&2
    exit 1
fi
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-portal-pw-test.XXXXXXXX")
daemon_pid=''
cleanup() {
    if [[ -n $daemon_pid ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT
cat > "$TEST_ROOT/test.conf" <<'CONF'
context.properties = {
    core.daemon = true
    core.name = pipewire-0
}
context.spa-libs = {
    support.* = support/libspa-support
    audio.convert.* = audioconvert/libspa-audioconvert
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-access args = { access.force = unrestricted } }
]
CONF
export PIPEWIRE_RUNTIME_DIR="$TEST_ROOT"
export PIPEWIRE_REMOTE="$TEST_ROOT/pipewire-0"
export JWM_PORTAL_TEST_PIPEWIRE=1
LD_LIBRARY_PATH="$PREFIX/lib:$PREFIX/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$PIPEWIRE_BIN" --config "$TEST_ROOT/test.conf" > "$TEST_ROOT/daemon.log" 2>&1 &
daemon_pid=$!
ready=false
for ((attempt=0; attempt<100; attempt++)); do
    if [[ -S $PIPEWIRE_REMOTE ]]; then
        ready=true
        break
    fi
    kill -0 "$daemon_pid" 2>/dev/null || break
    sleep 0.05
done
if [[ $ready != true ]]; then
    cat "$TEST_ROOT/daemon.log" >&2
    echo 'test-portal-pipewire: private daemon did not start' >&2
    exit 1
fi
cd "$ROOT"
bash scripts/test.sh -p jwm-portal --bin jwm-portal -- \
    --ignored pipewire_stream::lifecycle_tests::private_daemon_stream_starts_and_stops
