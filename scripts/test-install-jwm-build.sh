#!/usr/bin/env bash
# Offline installer build recovery and artifact-path regression tests.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-build-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPO="$TEST_ROOT/project space"
mkdir -p "$REPO/scripts" "$REPO/bars/tao_glow_bar" "$REPO/bridge/dist" "$TEST_ROOT/bin"
cp "$ROOT/scripts/install_jwm_scripts.sh" "$REPO/scripts/"
touch "$REPO/bars/tao_glow_bar/Cargo.toml" "$REPO/bridge/Cargo.toml" "$REPO/bridge/dist/org.freedesktop.Notifications.service"
cat > "$TEST_ROOT/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s' "$1" >> "$CASE_DIR/cargo.log"
printf ' <%s>' "${@:2}" >> "$CASE_DIR/cargo.log"
printf '\n' >> "$CASE_DIR/cargo.log"
command=$1
shift
target= profile=debug package=jwm
while (($#)); do
    case "$1" in
        --target-dir) target=$2; shift 2 ;;
        --release) profile=release; shift ;;
        -p) package=$2; shift 2 ;;
        *) shift ;;
    esac
done
[[ $command == build ]] || exit 0
if [[ $package == jwm-bridge && $SCENARIO == bridge-failure ]]; then
    echo 'error: bridge compilation failure' >&2
    exit 101
fi
if [[ $package == jwm && $SCENARIO != success && $SCENARIO != tee-success && $SCENARIO != bridge-failure && $SCENARIO != install-failure ]]; then
    count=0
    [[ ! -f $CASE_DIR/count ]] || read -r count < "$CASE_DIR/count"
    count=$((count + 1))
    printf '%s\n' "$count" > "$CASE_DIR/count"
    if [[ $SCENARIO == ordinary ]]; then
        echo 'error: ordinary compilation failure' >&2
        exit 101
    fi
    if [[ $SCENARIO == repeated || $SCENARIO == tee-cargo || $count == 1 ]]; then
        printf "error: couldn't read %s/%s/build/xcb-123/out/randr.rs: No such file or directory (os error 2)\n" "$target" "$profile" >&2
        echo 'error: could not compile `xcb` (lib) due to 1 previous error' >&2
        exit 101
    fi
fi
mkdir -p "$target/$profile"
for binary in jwm jwm-tool jwm-support jwm-remote jwm-bridge; do
    printf '#!/bin/sh\nexit 0\n' > "$target/$profile/$binary"
    chmod +x "$target/$profile/$binary"
done
STUB
cat > "$TEST_ROOT/bin/sudo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CASE_DIR/system.log"
if [[ $SCENARIO == install-failure && $* == *jwm-remote* ]]; then
    exit 73
fi
STUB
cat > "$TEST_ROOT/bin/install" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CASE_DIR/system.log"
STUB
cat > "$TEST_ROOT/bin/tee" <<'STUB'
#!/usr/bin/env bash
/usr/bin/tee "$@"
case "$SCENARIO" in
    tee-cargo|tee-success) exit 74 ;;
esac
STUB
chmod +x "$TEST_ROOT/bin/"*
fail() { printf 'test-install-jwm-build: FAIL: %s\n' "$*" >&2; exit 1; }
run_case() {
    local scenario=$1 mode=$2 target=$3 expected=$4 status=0
    export CASE_DIR="$TEST_ROOT/$scenario-$mode" SCENARIO=$scenario
    mkdir -p "$CASE_DIR/home/.cargo/bin" "$CASE_DIR/config/jwm"
    local binary
    for binary in jwm jwm-tool jwm-support jwm-remote; do
        printf 'old %s\n' "$binary" > "$CASE_DIR/home/.cargo/bin/$binary"
    done
    printf '[status_bar]\nname = "old"\n' > "$CASE_DIR/config/jwm/config_x11.toml"
    cp "$CASE_DIR/config/jwm/config_x11.toml" "$CASE_DIR/config/jwm/config_wayland.toml"
    TMPDIR="$CASE_DIR" PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config" CARGO_TARGET_DIR="$target" \
        bash "$REPO/scripts/install_jwm_scripts.sh" --mode "$mode" --jobs 2 > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == "$expected" ]] || fail "$scenario/$mode status $status, expected $expected"
    for binary in jwm jwm-tool jwm-support jwm-remote; do
        if [[ $expected == 0 ]]; then
            [[ ! -e $CASE_DIR/home/.cargo/bin/$binary ]] || fail "successful migration retained $binary"
        else
            [[ $(cat "$CASE_DIR/home/.cargo/bin/$binary") == "old $binary" ]] || fail "failed install removed $binary"
        fi
    done
    if compgen -G "$CASE_DIR/jwm-build.*" >/dev/null; then
        fail 'owned build log was not cleaned'
    fi
    local resolved=$target
    [[ $target == /* ]] || resolved="$REPO/$target"
    grep -Fq "install <--locked> <--target-dir> <$resolved/bar-install/tao_glow_bar>" "$CASE_DIR/cargo.log" || fail 'bar cache or lock missing'
    grep -Fq "build <--locked> <--target-dir> <$resolved>" "$CASE_DIR/cargo.log" || fail 'workspace target missing'
    if [[ $expected == 0 ]]; then
        grep -Fq '<-p> <jwm-bridge>' "$CASE_DIR/cargo.log" || fail 'bridge build missing'
        grep -Fq "$resolved/$mode/jwm" "$CASE_DIR/system.log" || fail 'artifact path mismatch'
    else
        if [[ $scenario != bridge-failure && $scenario != install-failure ]]; then
            [[ ! -e $CASE_DIR/system.log ]] || fail 'failed build installed system files'
        fi
    fi
    local cleans
    cleans=$(grep -c '^clean ' "$CASE_DIR/cargo.log" || true)
    case "$scenario" in
        missing|repeated)
            [[ $cleans == 1 ]] || fail 'recovery did not clean exactly once'
            if [[ $mode == release ]]; then
                grep -Fq "clean <-p> <xcb> <--release> <--target-dir> <$resolved>" "$CASE_DIR/cargo.log" || fail 'release clean scope mismatch'
            else
                grep -Fq "clean <-p> <xcb> <--profile> <dev> <--target-dir> <$resolved>" "$CASE_DIR/cargo.log" || fail 'debug clean scope mismatch'
            fi
            ;;
        *) [[ $cleans == 0 ]] || fail 'unexpected clean' ;;
    esac
    if [[ $scenario == repeated ]]; then
        [[ $(cat "$CASE_DIR/count") == 2 ]] || fail 'recovery retried more than once'
    fi
}
run_case missing release 'custom target' 0
run_case missing debug "$TEST_ROOT/absolute target" 0
run_case success debug 'normal target' 0
run_case ordinary release 'failed target' 101
run_case repeated release 'repeated target' 101
run_case tee-cargo release 'tee cargo target' 101
run_case tee-success release 'tee success target' 74
run_case bridge-failure release 'bridge failed target' 101
run_case install-failure debug 'install failed target' 73
# A bar-only/no-op invocation must never remove an existing JWM installation.
export CASE_DIR="$TEST_ROOT/skip-jwm" SCENARIO=success
mkdir -p "$CASE_DIR/home/.cargo/bin"
for binary in jwm jwm-tool jwm-support jwm-remote; do
    printf 'old %s\n' "$binary" > "$CASE_DIR/home/.cargo/bin/$binary"
done
PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" \
    bash "$REPO/scripts/install_jwm_scripts.sh" --skip-jwm --skip-bar > "$CASE_DIR/output.log" 2>&1
for binary in jwm jwm-tool jwm-support jwm-remote; do
    [[ $(cat "$CASE_DIR/home/.cargo/bin/$binary") == "old $binary" ]] || fail "--skip-jwm removed $binary"
done
printf 'test-install-jwm-build: PASS\n'
