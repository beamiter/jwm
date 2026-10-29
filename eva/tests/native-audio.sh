#!/usr/bin/env bash
# Deterministic native PipeWire integration: synthetic input + silent null sink.
# This never connects to the user's session graph or hardware audio nodes.
set -euo pipefail
cd "$(dirname "$0")/../.."
for executable in pipewire pipewire-media-session pw-cli; do
    command -v "$executable" >/dev/null || { echo "missing test dependency: $executable" >&2; exit 1; }
done
runtime_dir=$(mktemp -d /tmp/jwm-eva-pw-XXXXXX)
chmod 700 "$runtime_dir"
pw_pid=0
manager_pid=0
cleanup() {
    local result=$?
    if (( manager_pid > 0 )); then kill "$manager_pid" 2>/dev/null || true; wait "$manager_pid" 2>/dev/null || true; fi
    if (( pw_pid > 0 )); then kill "$pw_pid" 2>/dev/null || true; wait "$pw_pid" 2>/dev/null || true; fi
    if (( result != 0 )) || [[ "${JWM_EVA_KEEP_AUDIO_TEST:-0}" == 1 ]]; then
        echo "native audio test artifacts: $runtime_dir" >&2
    else
        rm -rf "$runtime_dir"
    fi
}
trap cleanup EXIT
cat > "$runtime_dir/pipewire.conf" <<'CONFIG'
context.properties = {
    core.daemon = true
    core.name = pipewire-0
    default.clock.rate = 24000
    default.clock.quantum = 480
    default.clock.min-quantum = 480
    default.clock.max-quantum = 480
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
    audiotestsrc = audiotestsrc/libspa-audiotestsrc
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-access }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-session-manager }
]
context.objects = [
    { factory = metadata args = { metadata.name = default } }
    { factory = spa-node-factory args = {
        factory.name = support.node.driver
        node.name = Dummy-Driver
        node.group = pipewire.dummy
        priority.driver = 20000
        node.driver = true
        node.always-process = true
    } }
    { factory = adapter args = {
        factory.name = support.null-audio-sink
        node.name = jwm-eva-test-sink
        node.group = pipewire.dummy
        node.driver = false
        node.want-driver = true
        media.class = Audio/Sink
        audio.channels = 1
        audio.position = [ MONO ]
        node.pause-on-idle = false
    } }
    { factory = adapter args = {
        factory.name = audiotestsrc
        node.name = jwm-eva-test-source
        node.group = pipewire.dummy
        node.driver = false
        node.want-driver = true
        media.class = Audio/Source
        audio.rate = 24000
        audio.channels = 1
        audio.position = [ MONO ]
        node.pause-on-idle = false
    } }
]
CONFIG
export XDG_RUNTIME_DIR="$runtime_dir"
export PIPEWIRE_RUNTIME_DIR="$runtime_dir"
export PIPEWIRE_REMOTE=pipewire-0
export JWM_EVA_AUDIO_TEST=1
server_binary=${JWM_EVA_PIPEWIRE_SERVER:-pipewire}
if [[ -z "${JWM_EVA_PIPEWIRE_SERVER:-}" && -n "${JWM_PIPEWIRE_PREFIX:-}" && -x "$JWM_PIPEWIRE_PREFIX/bin/pipewire" ]]; then
    server_binary="$JWM_PIPEWIRE_PREFIX/bin/pipewire"
fi
if [[ -n "${JWM_PIPEWIRE_PREFIX:-}" && "$server_binary" == "$JWM_PIPEWIRE_PREFIX/bin/pipewire" ]]; then
    LD_LIBRARY_PATH="$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$server_binary" -c "$runtime_dir/pipewire.conf" >"$runtime_dir/server.log" 2>&1 &
else
    "$server_binary" -c "$runtime_dir/pipewire.conf" >"$runtime_dir/server.log" 2>&1 &
fi
pw_pid=$!
for _ in {1..100}; do
    [[ -S "$runtime_dir/pipewire-0" ]] && break
    kill -0 "$pw_pid" 2>/dev/null || { cat "$runtime_dir/server.log" >&2; exit 1; }
    sleep 0.02
done
pipewire-media-session >"$runtime_dir/manager.log" 2>&1 &
manager_pid=$!
# Readiness is an actual graph roundtrip, not the mere presence of its socket.
for _ in {1..100}; do
    if pw-cli ls Node 2>/dev/null | rg -q 'jwm-eva-test-source'; then break; fi
    sleep 0.02
done
# audiotestsrc defaults to an asynchronous driver without updating Clock.
# Run it synchronously under Dummy-Driver so both audio and graph timing are
# produced by the real graph rather than by a test-side playback estimate.
source_id=$(pw-cli ls Node | awk '/id [0-9]+,/{id=$2;gsub(",", "", id)} /node.name = "jwm-eva-test-source"/{print id}')
[[ "$source_id" =~ ^[0-9]+$ ]] || { echo "synthetic source missing" >&2; exit 1; }
pw-cli set-param "$source_id" Props '{ live = false }' > /dev/null
pw-dump > "$runtime_dir/graph.json" 2>/dev/null || true
if [[ -n "${JWM_EVA_AUDIO_TEST_BINARY:-}" ]]; then
    "$JWM_EVA_AUDIO_TEST_BINARY" --ignored --test-threads=1 --nocapture
else
    cargo test -p jwm-eva --lib --offline -- --ignored --test-threads=1 --nocapture
fi
# The native streams must have been destroyed at the resource boundary.
if pw-cli ls Node | rg -q 'node.name = "jwm-eva\.(capture|playback)"'; then
    echo "Eva left an audio stream after capture.stop/playback.finish" >&2
    exit 1
fi
