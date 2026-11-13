#!/usr/bin/env bash
# Real executable acceptance, not the in-process viewer/TCP test harness.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v timeout >/dev/null
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
export JWM_REQUIRE_HEADLESS_GL=1 LIBGL_ALWAYS_SOFTWARE=1 EGL_PLATFORM=surfaceless
# Permission failure is a blocked/failing acceptance run, NEVER a skipped pass.
python3 - <<'PY'
import os, socket
path = os.path.join(os.environ['TMPDIR'], 'probe.sock')
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
    listener.bind(path)
    listener.listen(1)
os.unlink(path)
PY
features=remote-wayland,backend-wayland-udev
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
cargo test --locked --no-default-features --features "$features" --lib --no-run
set +e
timeout --signal=TERM --kill-after=10s 90s \
  cargo test --locked --no-default-features --features "$features" --lib \
    cli_process_integration -- --ignored --test-threads=1 --nocapture \
    > "$run_root/tests.log" 2>&1 &
runner=$!
wait "$runner"
status=$?
runner=""
set -e
cat "$run_root/tests.log"
[[ "$status" -eq 0 ]] || exit "$status"
python3 - "$run_root/tests.log" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
assert re.search(r'test result: ok\. 3 passed; 0 failed; 0 ignored;', text), \
    'all three independent CLI acceptance cases must really execute'
names = {
    'native_cli_flags_reach_real_viewer_and_preserve_lease',
    'native_cli_requires_host_opt_in',
    'native_cli_rejects_old_local_v1_without_fallback',
}
for name in names:
    assert re.search(r'test [^\n]*::' + name + r' \.\.\. ok', text), name
print('Independent CLI-process acceptance: 3 executed, 0 skipped; strict GL required.')
PY
