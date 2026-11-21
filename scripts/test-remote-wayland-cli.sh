#!/usr/bin/env bash
# Real executable acceptance, not the in-process viewer/TCP test harness.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v timeout >/dev/null
test_filter=cli_process_integration
features=remote-wayland,backend-wayland-udev
needs_private_bus=false
case "${1-}" in
  --gedit)
    [[ "$#" -eq 1 ]]
    test_filter=actual_gedit_types_chinese_and_saves_selected_document_through_managed_wayland
    command -v dbus-run-session >/dev/null
    needs_private_bus=true
    JWM_REMOTE_GEDIT_BIN="${JWM_REMOTE_GEDIT_BIN:-$(command -v gedit)}"
    [[ "$JWM_REMOTE_GEDIT_BIN" = /* && -x "$JWM_REMOTE_GEDIT_BIN" ]]
    export JWM_REMOTE_GEDIT_BIN
    ;;
  --firefox)
    [[ "$#" -eq 1 ]]
    test_filter=actual_firefox_remote_address_tabs_and_wheel
    command -v dbus-run-session >/dev/null
    needs_private_bus=true
    JWM_REMOTE_FIREFOX_BIN="${JWM_REMOTE_FIREFOX_BIN:-$(command -v firefox)}"
    [[ "$JWM_REMOTE_FIREFOX_BIN" = /* && -x "$JWM_REMOTE_FIREFOX_BIN" ]]
    export JWM_REMOTE_FIREFOX_BIN
    ;;
  --blender)
    [[ "$#" -eq 1 ]]
    test_filter=actual_blender_remote_orbit_zoom_and_shading_popup
    command -v dbus-run-session >/dev/null
    needs_private_bus=true
    JWM_REMOTE_BLENDER_BIN="${JWM_REMOTE_BLENDER_BIN:-$(command -v blender)}"
    [[ "$JWM_REMOTE_BLENDER_BIN" = /* && -x "$JWM_REMOTE_BLENDER_BIN" ]]
    export JWM_REMOTE_BLENDER_BIN
    ;;
  --gpu-capture)
    [[ "$#" -eq 2 && "$2" = /* && -c "$2" ]]
    test_filter=native_cli_wayland_gpu_capture_encodes_and_falls_back_on_same_source
    features+=,remote-h264
    export JWM_TEST_WAYLAND_GPU_CAPTURE_NODE="$2"
    ;;
  --gpu-viewer)
    [[ "$#" -eq 2 && "$2" = /* && -c "$2" ]]
    test_filter=native_cli_wayland_gpu_viewer_presents_and_recovers_from_import_rejection
    features+=,remote-h264
    export JWM_TEST_WAYLAND_GPU_CAPTURE_NODE="$2"
    ;;
  --gpu-viewer-4k)
    [[ "$#" -eq 2 && "$2" = /* && -c "$2" ]]
    test_filter=native_cli_wayland_gpu_viewer_presents_owned_4k_region
    features+=,remote-h264
    export JWM_TEST_WAYLAND_GPU_CAPTURE_NODE="$2"
    ;;
  '') [[ "$#" -eq 0 ]] ;;
  *) echo 'Usage: test-remote-wayland-cli.sh [--gedit|--firefox|--blender|--gpu-capture RENDER_NODE|--gpu-viewer RENDER_NODE|--gpu-viewer-4k RENDER_NODE]' >&2; exit 2 ;;
esac
run_root="$(mktemp -d -t jwm-cli-acceptance.XXXXXXXX)"
chmod 700 "$run_root"
runner=""
cleanup() {
  if [[ -n "$runner" ]]; then
    # GNU timeout forwards termination to its owned test process group.
    kill -TERM "$runner" 2>/dev/null || true
    wait "$runner" 2>/dev/null || true
  fi
  rm -rf -- "$run_root"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export TMPDIR="$run_root" XDG_RUNTIME_DIR="$run_root"
export JWM_REQUIRE_HEADLESS_GL=1 EGL_PLATFORM=surfaceless
if [[ -n "${JWM_TEST_WAYLAND_GPU_CAPTURE_NODE:-}" && "${1-}" == --gpu-* ]]; then
  unset LIBGL_ALWAYS_SOFTWARE
else
  export LIBGL_ALWAYS_SOFTWARE=1
fi
# Permission failure is a blocked/failing acceptance run, NEVER a skipped pass.
python3 - <<'PY'
import os, socket
path = os.path.join(os.environ['TMPDIR'], 'probe.sock')
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
    listener.bind(path)
    listener.listen(1)
os.unlink(path)
PY
cargo build --locked --no-default-features --features "$features" \
  --bin jwm-remote --message-format=json > "$run_root/build.jsonl"
JWM_REMOTE_CLI_TEST_BIN="$(python3 - "$run_root/build.jsonl" <<'PY'
import json, os, sys
paths = [r['executable'] for line in open(sys.argv[1])
         if (r := json.loads(line)).get('reason') == 'compiler-artifact'
         and r.get('target', {}).get('name') == 'jwm-remote'
         and r.get('executable')]
assert len(paths) == 1, 'need exactly one actual CLI executable artifact'
print(os.path.realpath(paths[0]))
PY
)"
export JWM_REMOTE_CLI_TEST_BIN
# Build outside the runtime timeout so a cold compiler is not mistaken for a
# hung protocol. Each test has its own bounded waits and child Drop cleanup.
cargo test --locked --no-default-features --features "$features" --lib --no-run \
  --message-format=json > "$run_root/test-build.jsonl"
test_binary="$(python3 - "$run_root/test-build.jsonl" <<'PY'
import json, os, sys
paths = [r['executable'] for line in open(sys.argv[1])
         if (r := json.loads(line)).get('reason') == 'compiler-artifact'
         and r.get('target', {}).get('name') == 'jwm'
         and r.get('profile', {}).get('test')
         and r.get('executable')]
assert len(paths) == 1, 'need exactly one library test executable'
print(os.path.realpath(paths[0]))
PY
)"
session_bus=()
if "$needs_private_bus"; then
  # The private bus has no service activation, including desktop portals,
  # keyrings and volume monitors from the interactive desktop.
  python3 - "$run_root" > "$run_root/dbus.conf" <<'PY'
import sys
from xml.sax.saxutils import escape

print(f'''
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir={escape(sys.argv[1])}</listen>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
''')
PY
  session_bus=(dbus-run-session --config-file="$run_root/dbus.conf" --)
fi
set +e
timeout --signal=TERM --kill-after=10s 120s "${session_bus[@]}" \
  "$test_binary" "$test_filter" --ignored --test-threads=1 --show-output \
    > "$run_root/tests.log" 2>&1 &
runner=$!
wait "$runner"
status=$?
runner=""
set -e
cat "$run_root/tests.log"
[[ "$status" -eq 0 ]] || exit "$status"
if [[ "$test_filter" != cli_process_integration ]]; then
  awk '/^test result: ok\. 1 passed; 0 failed; 0 ignored;/ { ran=1 } END { exit !ran }' "$run_root/tests.log"
  exit
fi
python3 - "$run_root/tests.log" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
names = {
    'native_cli_explicit_long_budget_expires_without_regrant',
    'native_cli_flags_reach_real_viewer_and_preserve_lease',
    'native_cli_managed_release_keeps_same_viewer',
    'native_cli_source_catalog_rebuilds_wayland_roles_and_retires_old_feedback',
    'native_cli_source_catalog_never_reopens_a_remapped_inactive_grant',
    'native_cli_switch_releases_old_grant_and_explicitly_controls_presented_target',
    'native_cli_wayland_gestures_hold_through_video_release_on_resize_and_switch',
    'native_cli_wayland_gestures_requires_authenticated_and_native_opt_in',
    'native_cli_wayland_mixed_scale_outputs_keep_capture_and_control_window_scoped',
    'native_cli_requires_host_opt_in',
    'native_cli_rejects_old_local_v1_without_fallback',
}
assert re.search(rf'test result: ok\. {len(names)} passed; 0 failed; 0 ignored;', text), \
    'all independent CLI acceptance cases must execute'
for name in names:
    assert re.search(r'test [^\n]*::' + name + r' \.\.\. ok', text), name
print(f'Independent CLI-process acceptance: {len(names)} executed, 0 skipped; strict GL required.')
PY
