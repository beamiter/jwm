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
for executable in pipewire pw-cli pw-dump pw-record python3 rg; do
    command -v "$executable" >/dev/null || { echo "missing test dependency: $executable" >&2; exit 1; }
done
session_manager=pipewire-media-session
wireplumber_data=
wireplumber_modules=
if command -v wireplumber >/dev/null && [[ "$(wireplumber --version)" == *'libwireplumber 0.5.'* ]]; then
    session_manager=wireplumber-0.5
    # Also support a staged installation without adding it to the user's
    # global data/module search paths. No configuration is loaded from HOME.
    manager_prefix=$(cd "$(dirname "$(command -v wireplumber)")/.." && pwd -P)
    wireplumber_data=${JWM_EVA_WIREPLUMBER_DATA_DIR:-$manager_prefix/share/wireplumber}
    wireplumber_modules=${JWM_EVA_WIREPLUMBER_MODULE_DIR:-}
    if [[ -z "$wireplumber_modules" ]]; then
        for directory in "$manager_prefix"/lib/wireplumber-0.5 "$manager_prefix"/lib64/wireplumber-0.5 "$manager_prefix"/lib/*/wireplumber-0.5; do
            if [[ -d "$directory" ]]; then wireplumber_modules=$directory; break; fi
        done
    fi
    [[ -f "$wireplumber_data/wireplumber.conf" && -d "$wireplumber_modules" ]] || {
        echo 'WirePlumber 0.5 test requires its installed wireplumber.conf, scripts and modules; set JWM_EVA_WIREPLUMBER_{DATA,MODULE}_DIR for a custom prefix' >&2; exit 1;
    }
elif command -v wireplumber >/dev/null && [[ "$(wireplumber --version)" == *'libwireplumber 0.4.'* ]]; then
    session_manager=wireplumber-0.4
elif ! command -v pipewire-media-session >/dev/null; then
    echo 'missing compatible session manager: install WirePlumber 0.4/0.5 or pipewire-media-session' >&2
    exit 1
fi
runtime_dir=$(mktemp -d /tmp/jwm-eva-pw-XXXXXX)
chmod 700 "$runtime_dir"
mkdir -m700 "$runtime_dir/config" "$runtime_dir/state" "$runtime_dir/data" "$runtime_dir/pulse"
mkdir -m700 "$runtime_dir/config/media-session.d"
pw_pid=0
manager_pid=0
monitor_pid=0
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
    stop_child "$monitor_pid"
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
        audio.channels = 2
        audio.position = [ FL FR ]
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
# pw-record uses the realtime client configuration name; keep it just as
# isolated as Eva's native client instead of falling back to user settings.
cp "$runtime_dir/config/client.conf" "$runtime_dir/config/client-rt.conf"
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
    { name = libwireplumber-module-si-audio-endpoint, type = module }
    { name = libwireplumber-module-default-nodes, type = module }
    { name = libwireplumber-module-default-nodes-api, type = module }
    { name = create-item.lua, type = script/lua }
    { name = policy-node.lua, type = script/lua }
]
CONFIG
if [[ "$session_manager" == wireplumber-0.5 ]]; then
    # Keep the installed version's component dependencies and settings schema,
    # but select only policy. Definitions of hardware components are inert:
    # the selected profile explicitly disables every monitor and external bus
    # integration. No stock main profile or config fragments are inherited.
    cp "$wireplumber_data/wireplumber.conf" "$runtime_dir/config/eva-native-wireplumber.conf"
    cat >> "$runtime_dir/config/eva-native-wireplumber.conf" <<'CONFIG'
override.context.properties = { support.dbus = false }
override.context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
]
wireplumber.profiles = {
    eva-native-test = {
        inherits = [ policy, mixin.systemwide-session, mixin.stateless ]
        hardware.audio = disabled
        hardware.bluetooth = disabled
        hardware.video-capture = disabled
        monitor.alsa = disabled
        monitor.alsa-midi = disabled
        monitor.bluez = disabled
        monitor.bluez-midi = disabled
        monitor.v4l2 = disabled
        monitor.libcamera = disabled
        support.dbus = disabled
        support.portal-permissionstore = disabled
        support.reserve-device = disabled
        support.logind = disabled
        script.client.access-portal = disabled
        metadata.sm-objects = disabled
    }
}
CONFIG
fi
server_binary=${JWM_EVA_PIPEWIRE_SERVER:-pipewire}
pw_cli_command=(pw-cli)
pw_dump_command=(pw-dump)
test_environment=()
if [[ -z "${JWM_EVA_PIPEWIRE_SERVER:-}" && -n "${JWM_PIPEWIRE_PREFIX:-}" && -x "$JWM_PIPEWIRE_PREFIX/bin/pipewire" ]]; then
    server_binary="$JWM_PIPEWIRE_PREFIX/bin/pipewire"
fi
# A staged distribution client has compiled-in /usr paths. Keep client module
# resolution paired with the PATH installation, independently of a private
# server. In particular, a system WirePlumber must not load the newer server's
# modules using its older system libpipewire.
client_path=$(command -v pipewire)
client_prefix=$(cd "$(dirname "$client_path")/.." && pwd -P)
for directory in "$client_prefix"/lib/pipewire-0.3 "$client_prefix"/lib64/pipewire-0.3 "$client_prefix"/lib/*/pipewire-0.3; do
    if [[ -d "$directory" ]]; then export PIPEWIRE_MODULE_DIR=$directory; break; fi
done
for directory in "$client_prefix"/lib/spa-0.2 "$client_prefix"/lib64/spa-0.2 "$client_prefix"/lib/*/spa-0.2; do
    if [[ -d "$directory" ]]; then export SPA_PLUGIN_DIR=$directory; break; fi
done
server_path=$(command -v "$server_binary")
server_prefix=$(cd "$(dirname "$server_path")/.." && pwd -P)
server_environment=(env)
for directory in "$server_prefix"/lib/pipewire-0.3 "$server_prefix"/lib64/pipewire-0.3 "$server_prefix"/lib/*/pipewire-0.3; do
    if [[ -d "$directory" ]]; then server_environment+=("PIPEWIRE_MODULE_DIR=$directory"); break; fi
done
for directory in "$server_prefix"/lib/spa-0.2 "$server_prefix"/lib64/spa-0.2 "$server_prefix"/lib/*/spa-0.2; do
    if [[ -d "$directory" ]]; then server_environment+=("SPA_PLUGIN_DIR=$directory"); break; fi
done
if [[ -n "${JWM_PIPEWIRE_PREFIX:-}" && "$server_binary" == "$JWM_PIPEWIRE_PREFIX/bin/pipewire" ]]; then
    # Old distribution pw-dump cannot serialize newer server parameter IDs.
    # Keep the inspection tools paired with the selected private server.
    for executable in pw-cli pw-dump; do
        [[ -x "$JWM_PIPEWIRE_PREFIX/bin/$executable" ]] || {
            echo "missing private PipeWire test tool: $executable" >&2; exit 1;
        }
    done
    pw_cli_command=("${server_environment[@]}" "LD_LIBRARY_PATH=$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$JWM_PIPEWIRE_PREFIX/bin/pw-cli")
    pw_dump_command=("${server_environment[@]}" "LD_LIBRARY_PATH=$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$JWM_PIPEWIRE_PREFIX/bin/pw-dump")
    # Eva's build.rs links its native test binary to this private library too.
    # Keep its modules paired with that library, while WirePlumber/pw-record
    # retain the PATH installation's client environment.
    test_environment=("${server_environment[@]}" "LD_LIBRARY_PATH=$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}")
    "${server_environment[@]}" "LD_LIBRARY_PATH=$JWM_PIPEWIRE_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$server_binary" -c "$runtime_dir/pipewire.conf" >"$runtime_dir/server.log" 2>&1 &
else
    "${server_environment[@]}" "$server_binary" -c "$runtime_dir/pipewire.conf" >"$runtime_dir/server.log" 2>&1 &
fi
pw_pid=$!
for _ in {1..100}; do
    [[ -S "$runtime_dir/pipewire-0" ]] && break
    kill -0 "$pw_pid" 2>/dev/null || { cat "$runtime_dir/server.log" >&2; exit 1; }
    sleep 0.02
done
if [[ "$session_manager" == wireplumber-0.5 ]]; then
    WIREPLUMBER_CONFIG_DIR="$runtime_dir/config" WIREPLUMBER_DATA_DIR="$wireplumber_data" \
        WIREPLUMBER_MODULE_DIR="$wireplumber_modules" wireplumber -c eva-native-wireplumber.conf \
        -p eva-native-test >"$runtime_dir/manager.log" 2>&1 &
elif [[ "$session_manager" == wireplumber-0.4 ]]; then
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
    # Record both sink monitor channels. A mono-only sink hides unpositioned
    # playback being routed to only one side of stereo headphones.
    # Older pw-record writes raw stdout directly and has no --raw switch;
    # newer versions require the switch to avoid a libsndfile/AU header.
    record_format_options=()
    record_help=$(pw-record --help)
    if [[ "$record_help" == *'--raw'* ]]; then record_format_options+=(--raw); fi
    pw-record "${record_format_options[@]}" --target jwm-eva-test-sink --rate 24000 --channels 2 \
        --channel-map stereo --format s16 \
        --properties 'node.name=jwm-eva-test-monitor stream.capture.sink=true' - \
        >"$runtime_dir/stereo.pcm" 2>"$runtime_dir/monitor.log" &
    monitor_pid=$!
    monitor_ready=false
    for _ in {1..100}; do
        kill -0 "$monitor_pid" 2>/dev/null || { cat "$runtime_dir/monitor.log" >&2; exit 1; }
        "${pw_dump_command[@]}" > "$runtime_dir/monitor-graph.json"
        if python3 - "$runtime_dir/monitor-graph.json" <<'PY'
import json
import sys

objects = json.load(open(sys.argv[1]))
nodes = {item["info"]["props"].get("node.name"): item["id"]
         for item in objects if item.get("type") == "PipeWire:Interface:Node"}
sink, monitor = (nodes.get(name) for name in ("jwm-eva-test-sink", "jwm-eva-test-monitor"))
links = [item["info"] for item in objects if item.get("type") == "PipeWire:Interface:Link"]
ready = sink is not None and monitor is not None and sum(
    link.get("output-node-id") == sink and link.get("input-node-id") == monitor
    and link.get("state") == "active" for link in links) == 2
sys.exit(0 if ready else 1)
PY
        then
            monitor_ready=true
            break
        fi
        sleep 0.02
    done
    [[ $monitor_ready == true ]] || { echo 'stereo sink monitor never linked' >&2; exit 1; }
    if [[ -n "${JWM_EVA_AUDIO_TEST_BINARY:-}" ]]; then
        test_command=("${test_environment[@]}" "$JWM_EVA_AUDIO_TEST_BINARY")
        test_separator=()
    else
        test_command=("${test_environment[@]}" cargo test --locked -p jwm-eva --lib --offline)
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
    stop_child "$monitor_pid"
    monitor_pid=0
    python3 - "$runtime_dir/stereo.pcm" <<'PY'
import math
import struct
import sys

pcm = open(sys.argv[1], "rb").read()
if pcm[:4] in (b"dns.", b".snd", b"RIFF", b"RF64", b"fLaC", b"OggS"):
    raise SystemExit("sink monitor contains a container header; raw PCM is required")
if not pcm or len(pcm) % 4:
    raise SystemExit("missing or incomplete stereo monitor PCM")
frames = list(struct.iter_unpack("<hh", pcm))
energy = [sum(frame[channel] ** 2 for frame in frames) for channel in (0, 1)]
rms = [math.sqrt(value / len(frames)) / 32768 for value in energy]
error = sum((left - right) ** 2 for left, right in frames)
if min(rms) < 0.001 or error > max(energy) * 0.0001:
    raise SystemExit(f"mono feedback must reach both stereo channels equally: RMS={rms}")
print(f"Stereo sink monitor verified: left RMS={rms[0]:.5f}, right RMS={rms[1]:.5f}")
PY
fi
# The native streams must have been destroyed at the resource boundary.
if "${pw_cli_command[@]}" ls Node | rg -q 'node.name = "jwm-eva\.(capture|playback)"'; then
    echo "Eva left an audio stream after capture.stop/playback.finish" >&2
    exit 1
fi

