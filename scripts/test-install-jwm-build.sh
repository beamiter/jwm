#!/usr/bin/env bash
# Offline installer build recovery and artifact-path regression tests.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-build-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPO="$TEST_ROOT/project space"
mkdir -p "$REPO/scripts" "$REPO/bars/tao_glow_bar" "$REPO/bars/tao_pixels_bar" "$REPO/bridge/dist" "$REPO/assets/wallpapers" "$TEST_ROOT/bin"
cp "$ROOT/scripts/install_jwm_scripts.sh" "$ROOT/scripts/install-wallpapers.sh" "$REPO/scripts/"
printf 'bundled fixture\n' > "$REPO/assets/wallpapers/01-fixture.png"
touch "$REPO/bars/tao_glow_bar/Cargo.toml" "$REPO/bridge/Cargo.toml" "$REPO/bridge/dist/org.freedesktop.Notifications.service"
touch "$REPO/bars/tao_pixels_bar/Cargo.toml"
cp "$ROOT/bridge/dist/jwm-player.desktop" "$ROOT/bridge/dist/jwm-video.desktop" "$REPO/bridge/dist/"
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
if [[ $package == jwm && $SCENARIO != success && $SCENARIO != tee-success && $SCENARIO != bridge-failure && $SCENARIO != install-failure && $SCENARIO != sudo-target && $SCENARIO != xdg-target && $SCENARIO != reload-failure && $SCENARIO != player-missing && $SCENARIO != player-install-failure ]]; then
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
for binary in jwm jwm-tool jwm-support jwm-remote jwm-bridge jwm-player; do
    [[ $SCENARIO != player-missing || $binary != jwm-player ]] || continue
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
if [[ $SCENARIO == player-install-failure && $* == *jwm-player* ]]; then
    exit 73
fi
STUB
cat > "$TEST_ROOT/bin/install" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CASE_DIR/system.log"
# Stub system-wide installs, but actually copy the isolated fixture image so
# this test can assert the wallpaper content (not just the logged invocation).
if [[ " $* " == *"/assets/wallpapers/"* ]]; then
    /usr/bin/install "$@"
fi
STUB
cat > "$TEST_ROOT/bin/tee" <<'STUB'
#!/usr/bin/env bash
/usr/bin/tee "$@"
case "$SCENARIO" in
    tee-cargo|tee-success) exit 74 ;;
esac
STUB
cat > "$TEST_ROOT/bin/id" <<'STUB'
#!/usr/bin/env bash
if [[ ${SCENARIO:-} == sudo-target && $* == -u ]]; then
    printf '0\n'
else
    /usr/bin/id "$@"
fi
STUB
cat > "$TEST_ROOT/bin/getent" <<'STUB'
#!/usr/bin/env bash
if [[ ${SCENARIO:-} == sudo-target && $* == 'passwd fixture-user' ]]; then
    printf 'fixture-user:x:1000:1000:Fixture:%s:/bin/bash\n' "$CASE_DIR/login-home"
else
    /usr/bin/getent "$@"
fi
STUB
cat > "$TEST_ROOT/bin/timeout" <<'STUB'
#!/usr/bin/env bash
# Never reach the host's live D-Bus service from an installer fixture.
printf 'bridge-reload %s\n' "$*" >> "$CASE_DIR/system.log"
if [[ $SCENARIO == reload-failure ]]; then exit 124; fi
STUB
chmod +x "$TEST_ROOT/bin/"*
fail() { printf 'test-install-jwm-build: FAIL: %s\n' "$*" >&2; exit 1; }
run_case() {
    local scenario=$1 mode=$2 target=$3 expected=$4 status=0
    export CASE_DIR="$TEST_ROOT/$scenario-$mode" SCENARIO=$scenario
    local sudo_user='' data_home=''
    [[ $scenario != sudo-target ]] || sudo_user=fixture-user
    [[ $scenario != xdg-target ]] || data_home="$CASE_DIR/custom data"
    mkdir -p "$CASE_DIR/home/.cargo/bin" "$CASE_DIR/config/jwm"
    local binary
    for binary in jwm jwm-tool jwm-support jwm-remote jwm-player; do
        printf 'old %s\n' "$binary" > "$CASE_DIR/home/.cargo/bin/$binary"
    done
    printf '[status_bar]\nname = "old"\n' > "$CASE_DIR/config/jwm/config_x11.toml"
    cp "$CASE_DIR/config/jwm/config_x11.toml" "$CASE_DIR/config/jwm/config_wayland.toml"
    printf '[Default Applications]\naudio/mpeg=original.desktop;\nvideo/mp4=original.desktop;\n' > "$CASE_DIR/config/mimeapps.list"
    cp "$CASE_DIR/config/mimeapps.list" "$CASE_DIR/original-mimeapps.list"
    TMPDIR="$CASE_DIR" SUDO_USER="$sudo_user" XDG_DATA_HOME="$data_home" PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config" CARGO_TARGET_DIR="$target" \
        bash "$REPO/scripts/install_jwm_scripts.sh" --mode "$mode" --jobs 2 --bar tao_glow_bar > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == "$expected" ]] || fail "$scenario/$mode status $status, expected $expected"
    cmp "$CASE_DIR/config/mimeapps.list" "$CASE_DIR/original-mimeapps.list" || fail 'installer changed shared MIME defaults'
    for binary in jwm jwm-tool jwm-support jwm-remote jwm-player; do
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
    [[ $(grep -c '<-j> <2>' "$CASE_DIR/cargo.log") -ge 2 ]] || fail 'jobs not passed as two arguments to build/install'
    if [[ $expected == 0 ]]; then
        grep -Fq '<-p> <jwm-bridge> <--bins>' "$CASE_DIR/cargo.log" || fail 'bridge/player build missing'
        grep -Fq "$resolved/$mode/jwm" "$CASE_DIR/system.log" || fail 'artifact path mismatch'
        grep -Fq "install -m755 $resolved/$mode/jwm-player /usr/local/bin/" "$CASE_DIR/system.log" || fail 'player artifact not installed'
        for desktop in jwm-player.desktop jwm-video.desktop; do
            grep -Fq "install -Dm644 $REPO/bridge/dist/$desktop /usr/local/share/applications/$desktop" "$CASE_DIR/system.log" || fail "player desktop entry missing: $desktop"
        done
        ! grep -Eq 'xdg-mime|/mimeapps\.list' "$CASE_DIR/system.log" || fail 'installer changed shared MIME defaults'
        if [[ $scenario == sudo-target ]]; then
            grep -Fq -- "-u fixture-user python3 - $CASE_DIR/config/jwm-mimeapps.list $REPO/bridge/dist" "$CASE_DIR/system.log" || fail 'MIME defaults did not run as login user'
            grep -Fq -- '-u fixture-user timeout 8s /usr/local/bin/jwm-bridge reload' "$CASE_DIR/system.log" || fail 'bridge reload did not run as login user'
        else
            grep -Fxq 'audio/mpeg=jwm-player.desktop;' "$CASE_DIR/config/jwm-mimeapps.list" || fail 'JWM music default missing'
            grep -Fxq 'video/mp4=jwm-video.desktop;' "$CASE_DIR/config/jwm-mimeapps.list" || fail 'JWM video default missing'
            grep -Fq 'bridge-reload 8s /usr/local/bin/jwm-bridge reload' "$CASE_DIR/system.log" || fail 'installed bridge did not reload its replaced D-Bus owner'
        fi
        if [[ $scenario == sudo-target ]]; then
            grep -Fq -- "-u fixture-user bash $REPO/scripts/install-wallpapers.sh --dest $CASE_DIR/config/jwm/wallpapers" "$CASE_DIR/system.log" || fail 'sudo wallpaper install did not switch user'
        else
            [[ $(cat "$CASE_DIR/config/jwm/wallpapers/01-fixture.png") == "bundled fixture" ]] || fail 'bundled wallpaper install missing'
        fi
    else
        if [[ $scenario != bridge-failure && $scenario != install-failure && $scenario != player-missing && $scenario != player-install-failure ]]; then
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
    if [[ $scenario == sudo-target ]]; then
        grep -Fq -- "-u fixture-user install -Dm644 $REPO/bridge/dist/org.freedesktop.Notifications.service $CASE_DIR/login-home/.local/share/dbus-1/services/org.freedesktop.Notifications.service" "$CASE_DIR/system.log" || fail 'sudo bridge activation did not run as login user in login home'
    elif [[ $scenario == xdg-target ]]; then
        grep -Fq "$CASE_DIR/custom data/dbus-1/services/org.freedesktop.Notifications.service" "$CASE_DIR/system.log" || fail 'bridge activation ignored XDG_DATA_HOME'
    fi
    if [[ $scenario == reload-failure ]]; then
        grep -Fq '运行中旧进程未能自动重载' "$CASE_DIR/output.log" || fail 'bridge reload failure was hidden'
    fi
    if [[ $scenario == repeated ]]; then
        [[ $(cat "$CASE_DIR/count") == 2 ]] || fail 'recovery retried more than once'
    fi
}
run_case missing release 'custom target' 0
run_case missing debug "$TEST_ROOT/absolute target" 0
run_case success debug 'normal target' 0
run_case sudo-target debug 'sudo target' 0
run_case xdg-target release 'xdg target' 0
run_case reload-failure release 'reload degraded target' 0
run_case ordinary release 'failed target' 101
run_case repeated release 'repeated target' 101
run_case tee-cargo release 'tee cargo target' 101
run_case tee-success release 'tee success target' 74
run_case bridge-failure release 'bridge failed target' 101
run_case install-failure debug 'install failed target' 73
run_case player-missing release 'missing player target' 1
run_case player-install-failure release 'player install failed target' 73
# Exercise the actual default and selection precedence through the installer,
# rather than always supplying the official bar explicitly.
check_bar_selection() {
    local name=$1 selected_by_env=$2 expected=$3
    shift 3
    export CASE_DIR="$TEST_ROOT/bar-$name" SCENARIO=success
    mkdir -p "$CASE_DIR/home/.cargo" "$CASE_DIR/config/jwm"
    printf '[status_bar]\nname = "custom_bar"\n' > "$CASE_DIR/config/jwm/config_x11.toml"
    cp "$CASE_DIR/config/jwm/config_x11.toml" "$CASE_DIR/config/jwm/config_wayland.toml"
    JWM_BAR_NAME="$selected_by_env" PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" \
        CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config" \
        CARGO_TARGET_DIR="$CASE_DIR/target" \
        bash "$REPO/scripts/install_jwm_scripts.sh" --skip-jwm "$@" > "$CASE_DIR/output.log" 2>&1
    [[ $(grep -c '^install ' "$CASE_DIR/cargo.log") == 1 ]] || fail "$name installed more than one bar"
    grep -Fq -- "--path> <$REPO/bars/$expected>" "$CASE_DIR/cargo.log" || fail "$name selected the wrong bar"
    local expected_config=custom_bar
    [[ $# == 0 ]] || expected_config=$expected
    grep -Fq "\"$expected_config\"" "$CASE_DIR/config/jwm/config_x11.toml" || fail "$name changed or failed to change the selected bar"
    grep -Fq "\"$expected_config\"" "$CASE_DIR/config/jwm/config_wayland.toml" || fail "$name ignored the Wayland bar configuration"
}
check_bar_selection default '' tao_glow_bar
check_bar_selection environment tao_pixels_bar tao_pixels_bar
check_bar_selection argument tao_glow_bar tao_pixels_bar --bar tao_pixels_bar
check_bar_selection argument-over-environment tao_pixels_bar tao_glow_bar --bar tao_glow_bar
# A bar-only/no-op invocation must never remove an existing JWM installation.
export CASE_DIR="$TEST_ROOT/skip-jwm" SCENARIO=success
mkdir -p "$CASE_DIR/home/.cargo/bin"
for binary in jwm jwm-tool jwm-support jwm-remote jwm-player; do
    printf 'old %s\n' "$binary" > "$CASE_DIR/home/.cargo/bin/$binary"
done
PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" \
    bash "$REPO/scripts/install_jwm_scripts.sh" --skip-jwm --skip-bar > "$CASE_DIR/output.log" 2>&1
for binary in jwm jwm-tool jwm-support jwm-remote jwm-player; do
    [[ $(cat "$CASE_DIR/home/.cargo/bin/$binary") == "old $binary" ]] || fail "--skip-jwm removed $binary"
done
# Reject malformed input before any build, installation or config generation.
check_invalid_args() {
    local status=0
    export CASE_DIR="$TEST_ROOT/invalid-$invalid_index" SCENARIO=success
    mkdir -p "$CASE_DIR/home"
    PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" \
        bash "$REPO/scripts/install_jwm_scripts.sh" "$@" > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == 2 ]] || fail "invalid arguments returned $status: $*"
    [[ ! -e $CASE_DIR/cargo.log && ! -e $CASE_DIR/system.log && ! -e $CASE_DIR/home/.config ]] || fail "invalid arguments caused side effects: $*"
    grep -q '\[ERROR\]' "$CASE_DIR/output.log" || fail "missing argument diagnostic: $*"
    ((invalid_index += 1))
}
invalid_index=0
check_invalid_args --unknown
JWM_BAR_NAME=not-a-bar check_invalid_args --skip-jwm
for option in --mode -m --bar -b --jobs -j; do
    check_invalid_args "$option"
    check_invalid_args "$option" ''
    check_invalid_args "$option" --skip-bar
done
for value in 0 -1 abc '2 --features backend-xcb' '2 3' 1.5; do
    check_invalid_args --jobs "$value"
done
printf 'test-install-jwm-build: PASS\n'
