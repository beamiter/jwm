#!/usr/bin/env bash
# No downloads or installs: verify opt-in mirror setup preserves Cargo config.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-bootstrap-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
fail() { printf 'test-bootstrap-deps: FAIL: %s\n' "$*" >&2; exit 1; }
mkdir -p "$TEST_ROOT/bin" "$TEST_ROOT/home/.cargo"
# No real system or network commands may be invoked by these options.
for command in sudo apt-get curl rustup; do
    printf '#!/bin/sh\necho unexpected-system-command >&2\nexit 99\n' > "$TEST_ROOT/bin/$command"
    chmod +x "$TEST_ROOT/bin/$command"
done
run() {
    HOME="$TEST_ROOT/home" CARGO_HOME="$TEST_ROOT/home/.cargo" PATH="$TEST_ROOT/bin:$PATH" \
        bash "$ROOT/scripts/bootstrap_deps.sh" --no-apt --no-rust --cn > "$TEST_ROOT/output.log" 2>&1
}
cfg="$TEST_ROOT/home/.cargo/config.toml"
run
grep -Fq 'replace-with = "rsproxy-sparse"' "$cfg" || fail 'fresh mirror configuration missing'
printf '\n[build]\njobs = 2\n' >> "$cfg"
cp "$cfg" "$TEST_ROOT/expected"
run
cmp "$cfg" "$TEST_ROOT/expected" || fail 'existing mirror configuration was overwritten'
printf '[build]\njobs = 3\n' > "$cfg"
cp "$cfg" "$TEST_ROOT/expected"
run
cmp "$cfg" "$TEST_ROOT/expected" || fail 'unrelated configuration was overwritten'
mv "$cfg" "$TEST_ROOT/symlink-target"
ln -s "$TEST_ROOT/symlink-target" "$cfg"
run
[[ -L $cfg ]] || fail 'config symlink was replaced'
cmp "$TEST_ROOT/symlink-target" "$TEST_ROOT/expected" || fail 'config symlink target changed'
rm "$cfg"
ln -s "$TEST_ROOT/missing-target" "$cfg"
run
[[ -L $cfg && ! -e $TEST_ROOT/missing-target ]] || fail 'dangling config symlink was followed'
mkdir -p "$TEST_ROOT/custom cargo"
HOME="$TEST_ROOT/home" CARGO_HOME="$TEST_ROOT/custom cargo" PATH="$TEST_ROOT/bin:$PATH" \
    bash "$ROOT/scripts/bootstrap_deps.sh" --no-apt --no-rust --cn > "$TEST_ROOT/custom.log" 2>&1
[[ -f $TEST_ROOT/custom\ cargo/config.toml ]] || fail 'CARGO_HOME was ignored'
printf 'test-bootstrap-deps: PASS\n'
