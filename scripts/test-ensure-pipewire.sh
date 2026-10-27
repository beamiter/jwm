#!/usr/bin/env bash
# Development metadata discovery without downloads or system changes.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-pipewire-env-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
mkdir -p "$TEST_ROOT/bin" "$TEST_ROOT/home"
fail() { printf 'test-ensure-pipewire: FAIL: %s\n' "$*" >&2; exit 1; }
cat > "$TEST_ROOT/bin/pkg-config" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ -n ${PKG_CONFIG_PATH:-} && -f $PKG_CONFIG_PATH/libpipewire-0.3.pc ]]; then
    case "$1" in
        --atleast-version=*) exit 0 ;;
        --exists) [[ -f $PKG_CONFIG_PATH/libspa-0.2.pc ]] ;;
        --modversion) printf '1.6.10\n' ;;
    esac
elif [[ ${TEST_SYSTEM_COMPLETE:-0} == 1 ]]; then
    [[ $1 != --modversion ]] || printf '1.4.9\n'
else
    exit 1
fi
STUB
for command in git meson ninja cc; do
    printf '#!/bin/sh\necho unexpected-build >&2\nexit 99\n' > "$TEST_ROOT/bin/$command"
done
chmod +x "$TEST_ROOT/bin/"*
run() {
    HOME="$TEST_ROOT/home" PATH="$TEST_ROOT/bin:$PATH" PKG_CONFIG_PATH='' \
        bash "$ROOT/scripts/ensure_pipewire.sh" --env
}

# Complete system headers need neither a private prefix nor build tools.
TEST_SYSTEM_COMPLETE=1 JWM_PIPEWIRE_PREFIX='' run > "$TEST_ROOT/system.env"
[[ ! -s $TEST_ROOT/system.env ]] || fail 'system installation gained a private rpath'

# Explicit prefix wins over an otherwise usable system install. Paths with
# spaces and all three common library layouts must survive environment output.
for layout in lib lib64 lib/x86_64-linux-gnu; do
    prefix="$TEST_ROOT/private prefix/$layout"
    pc="$prefix/$layout/pkgconfig"
    mkdir -p "$pc"
    touch "$pc/libpipewire-0.3.pc" "$pc/libspa-0.2.pc"
    TEST_SYSTEM_COMPLETE=1 JWM_PIPEWIRE_PREFIX="$prefix" run > "$TEST_ROOT/prefix.env"
    grep -Fxq "JWM_PIPEWIRE_PREFIX=$prefix" "$TEST_ROOT/prefix.env" || fail "$layout prefix lost"
    grep -Fxq "PKG_CONFIG_PATH=$pc" "$TEST_ROOT/prefix.env" || fail "$layout metadata lost"
done

# A cached private prefix supports hosts with runtime packages only.
cached="$TEST_ROOT/home/.cache/jwm/pipewire-prefix/lib/pkgconfig"
mkdir -p "$cached"
touch "$cached/libpipewire-0.3.pc" "$cached/libspa-0.2.pc"
TEST_SYSTEM_COMPLETE=0 JWM_PIPEWIRE_PREFIX='' run > "$TEST_ROOT/cached.env"
grep -Fxq "PKG_CONFIG_PATH=$cached" "$TEST_ROOT/cached.env" || fail 'cached prefix not reused'

# Partial development metadata cannot be mistaken for a complete prefix.
rm "$cached/libspa-0.2.pc"
if TEST_SYSTEM_COMPLETE=0 JWM_PIPEWIRE_PREFIX='' run > "$TEST_ROOT/partial.env" 2> "$TEST_ROOT/partial.log"; then
    fail 'incomplete SPA metadata accepted'
fi
grep -q unexpected-build "$TEST_ROOT/partial.log" || fail 'missing metadata did not request a build'

# Reject unusable prefixes before they can trigger a download or emit GITHUB_ENV.
if JWM_PIPEWIRE_PREFIX=relative/path run > "$TEST_ROOT/relative.env" 2> "$TEST_ROOT/relative.log"; then
    fail 'relative prefix accepted'
fi
grep -q 'absolute path' "$TEST_ROOT/relative.log" || fail 'relative prefix diagnostic missing'
printf 'test-ensure-pipewire: PASS\n'
