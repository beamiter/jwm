#!/usr/bin/env bash
# Run a prebuilt Wayland test executable against an already running owned P09 host.
set -euo pipefail
if [[ $# -ne 7 && $# -ne 9 ]]; then
  echo "Usage: $0 LIB_TEST_BIN VIEWER_BIN RENDER_NODE HOST:PORT KEY_FILE RUN_ID NEW_EVIDENCE_DIR [WARMUP_SECONDS OBSERVE_SECONDS]" >&2
  exit 2
fi
test_bin=$1
viewer_bin=$2
node=$3
endpoint=$4
key=$5
run_id=$6
evidence=$7
warmup=${8:-0}
observe=${9:-5}
[[ $test_bin = /* && -x $test_bin && $viewer_bin = /* && -x $viewer_bin ]]
[[ $node = /* && -c $node && $key = /* && -r $key && $evidence = /* && ! -e $evidence ]]
[[ $run_id =~ ^[1-9][0-9]{0,2}$ && $warmup =~ ^(0|[1-9][0-9]{0,2})$ && $observe =~ ^[1-9][0-9]{0,3}$ ]]
(( run_id <= 255 && warmup <= 120 && observe >= 3 && observe <= 3600 ))
if (( observe > 120 && run_id > 15 )); then
  echo 'Observations over 120 seconds require the 20-bit P09 painter and a run ID from 1 to 15.' >&2
  exit 2
fi

runtime=$(mktemp -d -t jwm-p09-wayland.XXXXXXXX)
trap 'rm -rf -- "$runtime"' EXIT
export XDG_RUNTIME_DIR="$runtime" TMPDIR="$runtime" EGL_PLATFORM=surfaceless
unset LIBGL_ALWAYS_SOFTWARE
export JWM_REQUIRE_HEADLESS_GL=1 JWM_REMOTE_CLI_TEST_BIN="$viewer_bin"
export JWM_TEST_WAYLAND_GPU_CAPTURE_NODE="$node" JWM_TEST_P09_ENDPOINT="$endpoint"
export JWM_TEST_P09_KEY_FILE="$key" JWM_TEST_P09_RUN_ID="$run_id" JWM_TEST_P09_EVIDENCE_DIR="$evidence"
export JWM_TEST_P09_WARMUP_SECONDS="$warmup" JWM_TEST_P09_OBSERVE_SECONDS="$observe"

case_name=remote::host::wayland_viewer_integration::host_control_integration::cli_process_integration::source_switching::gpu_4k::marker::native_cli_wayland_gpu_viewer_observes_p09_4k_marker
timeout --signal=TERM --kill-after=10s "$((warmup + observe + 40))s" \
  "$test_bin" --exact "$case_name" --ignored --test-threads=1 --nocapture
[[ -s $evidence/observed.jsonl && -s $evidence/viewer-events.jsonl && -s $evidence/viewer-clock.jsonl ]]
