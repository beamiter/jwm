#!/usr/bin/env bash
# Deterministic native PipeWire integration: synthetic input + silent null sink.
# This never connects to the user's session graph or hardware audio nodes.
set -euo pipefail
cd "$(dirname "$0")/../.."
fixture_only=false
case "${1:-}" in
    --fixture-only) fixture_only=true; shift ;;
    '') ;;
    *) echo 'usage: native-audio.sh [--fixture-only]' >&2; exit 2 ;;
esac
[[ $# == 0 ]] || { echo 'usage: native-audio.sh [--fixture-only]' >&2; exit 2; }
graph_rate=${JWM_EVA_TEST_GRAPH_RATE:-24000}
case "$graph_rate" in
    24000|48000) ;;
    *) echo 'JWM_EVA_TEST_GRAPH_RATE must be 24000 or 48000' >&2; exit 2 ;;
esac
for executable in pipewire pw-cli pw-dump python3 rg; do
    command -v "$executable" >/dev/null || { echo "missing test dependency: $executable" >&2; exit 1; }
done
session_manager=pipewire-media-session
if command -v wireplumber >/dev/null && [[ "$(wireplumber --version)" == *'libwireplumber 0.4.'* ]]; then
    session_manager=wireplumber
elif ! command -v pipewire-media-session >/dev/null; then
    echo 'missing compatible session manager: install WirePlumber 0.4 (Ubuntu 24.04) or pipewire-media-session' >&2
    exit 1
fi
runtime_dir=$(mktemp -d /tmp/jwm-eva-pw-XXXXXX)
chmod 700 "$runtime_dir"
mkdir -m700 "$runtime_dir/config" "$runtime_dir/state" "$runtime_dir/data" "$runtime_dir/pulse"
mkdir -m700 "$runtime_dir/config/media-session.d"
pw_pid=0
manager_pid=0
stop_child() {
    local child=$1
    (( child > 0 )) || return 0
    kill "$child" 2>/dev/null || true
    for _ in {1..100}; do
        kill -0 "$child" 2>/dev/null || break
        sleep 0.02
    done
    if kill -0 "$child" 2>/dev/null; then kill -KILL "$child" 2>/dev/null || true; fi
    wait "$child" 2>/dev/null || true
}
cleanup() {
    local result=$?
    stop_child "$manager_pid"
    stop_child "$pw_pid"
    if (( result != 0 )) || [[ "${JWM_EVA_KEEP_AUDIO_TEST:-0}" == 1 ]]; then
        echo "native audio test artifacts: $runtime_dir" >&2
    else
        rm -rf "$runtime_dir"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cat > "$runtime_dir/pipewire.conf" <<CONFIG
context.properties = {
    core.daemon = true
    core.name = pipewire-0
    default.clock.rate = $graph_rate
    default.clock.allowed-rates = [ $graph_rate ]
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
        audio.rate = $graph_rate
        audio.channels = 1
        audio.position = [ MONO ]
        node.pause-on-idle = false
    } }
]
CONFIG
# Never inherit a PulseAudio route or a user PipeWire configuration. In
# particular, PULSE_SERVER would make automatic backend selection choose PA
# instead of this private graph. Leave it absent so the only PA probe is the
# nonexistent pulse/native socket under our own XDG_RUNTIME_DIR.
for audio_variable in "${!PULSE_@}" "${!PIPEWIRE_@}" "${!WIREPLUMBER_@}"; do
    [[ -z "$audio_variable" ]] || unset "$audio_variable"
done
export XDG_RUNTIME_DIR="$runtime_dir"
export XDG_CONFIG_HOME="$runtime_dir/config"
export XDG_STATE_HOME="$runtime_dir/state"
export XDG_DATA_HOME="$runtime_dir/data"
export XDG_CONFIG_DIRS="$runtime_dir/config"
export XDG_DATA_DIRS="$runtime_dir/data"
export PIPEWIRE_RUNTIME_DIR="$runtime_dir"
export PIPEWIRE_REMOTE=pipewire-0
export PIPEWIRE_CONFIG_DIR="$runtime_dir/config"
export PULSE_RUNTIME_PATH="$runtime_dir/pulse"
export PULSE_STATE_PATH="$runtime_dir/state"
export PULSE_CONFIG_PATH="$runtime_dir/config"
export PULSE_CLIENTCONFIG="$runtime_dir/config/pulse-client.conf"
export PULSE_COOKIE="$runtime_dir/pulse/unused-cookie"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime_dir/no-session-bus"
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$runtime_dir/no-system-bus"
export JWM_EVA_AUDIO_TEST=1
unset JWM_EVA_PULSE_TEST
printf 'autospawn=no\n' > "$PULSE_CLIENTCONFIG"
cat > "$runtime_dir/config/client.conf" <<'CONFIG'
context.properties = { support.dbus = false }
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-session-manager }
]
CONFIG
# Only link the synthetic nodes. The stock media-session configuration may
# enable ALSA, Bluetooth, camera or portal discovery and must not be loaded.
cat > "$runtime_dir/config/media-session.d/media-session.conf" <<'CONFIG'
context.properties = { support.dbus = false }
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-session-manager }
]
session.modules = { default = [ metadata default-nodes policy-node ] }
CONFIG
# media-session 0.4 resolves both -c and its fallback client configuration
# relative to the media-session.d namespace, even when -c looks absolute.
cp "$runtime_dir/config/client.conf" "$runtime_dir/config/media-session.d/client.conf"
# WirePlumber 0.4 provides the same native linking policy on distributions
# without media-session. Load only the needed modules/scripts, never main.lua,
# bluetooth.lua or any hardware/portal monitor from the desktop configuration.
# 0.5 has a different policy/configuration API and must not silently use this.
cat > "$runtime_dir/config/eva-native-wireplumber.conf" <<'CONFIG'
context.properties = {
    support.dbus = false
    wireplumber.script-engine = lua-scripting
    wireplumber.export-core = false
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-session-manager }
]
wireplumber.components = [
    { name = libwireplumber-module-lua-scripting, type = module }
    { name = libwireplumber-module-si-node, type = module }
    { name = libwireplumber-module-si-audio-adapter, type = module }
    { name = libwireplumber-module-si-standard-link, type = module }
    { name = libwireplumber-module-default-nodes, type = module }
    { name = libwireplumber-module-default-nodes-api, type = module }
    { name = create-item.lua, type = script/lua }
    { name = policy-node.lua, type = script/lua }
]
CONFIG
server_binary=${JWM_EVA_PIPEWIRE_SERVER:-pipewire}
pw_cli_command=(pw-cli)
pw_dump_command=(pw-dump)
if [[ -z "${JWM_EVA_PIPEWIRE_SERVER:-}" && -n "${JWM_PIPEWIRE_PREFIX:-}" && -x "$JWM_PIPEWIRE_PREFIX/bin/pipewire" ]]; then
    server_binary="$JWM_PIPEWIRE_PREFIX/bin/pipewire"
fi
if [[ -n "${JWM_PIPEWIRE_PREFIX:-}" && "$server_binary" == "$JWM_PIPEWIRE_PREFIX/bin/pipewire" ]]; then
    # Old distribution pw-dump cannot serialize newer server parameter IDs.
    # Keep the inspection tools paired with the selected private server.
    for executable in pw-cli pw-dump; do
        [[ -x "$JWM_PIPEWIRE_PREFIX/bin/$executable" ]] || {
            echo "missing private PipeWire test tool: $executable" >&2; exit 1;
        }
    done
    pw_cli_command=(env "LD_LIBRARY_PATH=$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$JWM_PIPEWIRE_PREFIX/bin/pw-cli")
    pw_dump_command=(env "LD_LIBRARY_PATH=$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$JWM_PIPEWIRE_PREFIX/bin/pw-dump")
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
if [[ "$session_manager" == wireplumber ]]; then
    WIREPLUMBER_CONFIG_DIR="$runtime_dir/config" wireplumber -c eva-native-wireplumber.conf >"$runtime_dir/manager.log" 2>&1 &
else
    pipewire-media-session -c media-session.conf >"$runtime_dir/manager.log" 2>&1 &
fi
manager_pid=$!
manager_ready=false
for _ in {1..100}; do
    kill -0 "$manager_pid" 2>/dev/null || { cat "$runtime_dir/manager.log" >&2; exit 1; }
    "${pw_cli_command[@]}" ls Client > "$runtime_dir/manager-clients.txt" 2>/dev/null
    if rg -Fq "pipewire.sec.pid = \"$manager_pid\"" "$runtime_dir/manager-clients.txt"; then
        manager_ready=true
        break
    fi
    sleep 0.02
done
[[ $manager_ready == true ]] || { echo 'private session manager never connected' >&2; cat "$runtime_dir/manager.log" >&2; exit 1; }
# Readiness is an actual graph roundtrip, not the mere presence of its socket.
for _ in {1..100}; do
    if "${pw_cli_command[@]}" ls Node 2>/dev/null | rg -q 'jwm-eva-test-source'; then break; fi
    sleep 0.02
done
# audiotestsrc defaults to an asynchronous driver without updating Clock.
# Run it synchronously under Dummy-Driver so both audio and graph timing are
# produced by the real graph rather than by a test-side playback estimate.
source_id=$("${pw_cli_command[@]}" ls Node | awk '/id [0-9]+,/{id=$2;gsub(",", "", id)} /node.name = "jwm-eva-test-source"/{print id}')
[[ "$source_id" =~ ^[0-9]+$ ]] || { echo "synthetic source missing" >&2; exit 1; }
"${pw_cli_command[@]}" set-param "$source_id" Props '{ live = false }' > /dev/null
"${pw_dump_command[@]}" > "$runtime_dir/graph.json" 2> "$runtime_dir/graph.log"
python3 - "$runtime_dir/graph.json" "$manager_pid" <<'PY'
import json
import sys

objects = json.load(open(sys.argv[1]))
nodes = {item.get("info", {}).get("props", {}).get("node.name")
         for item in objects if item.get("type") == "PipeWire:Interface:Node"}
expected = {"Dummy-Driver", "jwm-eva-test-source", "jwm-eva-test-sink"}
if nodes != expected or any(item.get("type") == "PipeWire:Interface:Device" for item in objects):
    raise SystemExit("private graph contains unexpected nodes/devices; refusing audio tests")
if not any(item.get("type") == "PipeWire:Interface:Client"
           and str(item.get("info", {}).get("props", {}).get("pipewire.sec.pid")) == sys.argv[2]
           for item in objects):
    raise SystemExit("private session manager is absent from the graph")
PY
if [[ $fixture_only == true ]]; then
    echo "Private PipeWire fixture verified; ${graph_rate}Hz synthetic source/sink (Rust tests intentionally not run)"
else
    echo "Native PipeWire graph rate: ${graph_rate}Hz (client PCM remains 24000Hz)"
    if [[ -n "${JWM_EVA_AUDIO_TEST_BINARY:-}" ]]; then
        test_command=("$JWM_EVA_AUDIO_TEST_BINARY")
        test_separator=()
    else
        test_command=(cargo test --locked -p jwm-eva --lib --offline)
        test_separator=(--)
    fi
    # Do not run every ignored test: PulseAudio has its own private fixture.
    for test_name in audio::native_tests::native_virtual_graph_roundtrip \
        feedback::tests::native_feedback_tone_finishes_at_real_drain; do
        listing=$("${test_command[@]}" "$test_name" "${test_separator[@]}" --ignored --exact --list --color=never)
        [[ "$listing" == *"$test_name: test"* ]] || {
            echo "missing required native PipeWire test: $test_name; zero tests is not a pass" >&2
            exit 1
        }
        "${test_command[@]}" "$test_name" "${test_separator[@]}" --ignored --exact --test-threads=1 --nocapture
    done
fi
# The native streams must have been destroyed at the resource boundary.
if "${pw_cli_command[@]}" ls Node | rg -q 'node.name = "jwm-eva\.(capture|playback)"'; then
    echo "Eva left an audio stream after capture.stop/playback.finish" >&2
    exit 1
fi

