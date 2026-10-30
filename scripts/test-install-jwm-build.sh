#!/usr/bin/env bash
# Offline installer build recovery and artifact-path regression tests.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-build-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPO="$TEST_ROOT/project space"
mkdir -p "$REPO/scripts" "$REPO/bars/tao_glow_bar" "$REPO/bars/tao_pixels_bar" "$REPO/bars/tauri_leptos_bar" "$REPO/crates/jwm_shell" "$REPO/bridge/dist" "$REPO/eva/dist" "$REPO/assets/wallpapers" "$REPO/packaging" "$TEST_ROOT/bin"
cp "$ROOT/packaging/69-jwm-touchpad-gestures.rules" "$REPO/packaging/"
cp "$ROOT/scripts/install-jwm.sh" "$ROOT/scripts/install-wallpapers.sh" "$ROOT/scripts/jwm-toggle-sidebar-mode" "$ROOT/scripts/install-eva.sh" "$REPO/scripts/"
cp "$ROOT/scripts/install-webkit-bar-launcher.sh" "$ROOT/scripts/run-with-webkit-transparent.sh" "$REPO/scripts/"
cp "$ROOT/scripts/check-webkitgtk-version.sh" "$REPO/scripts/"
cp "$ROOT/eva/dist/eva.toml" "$ROOT/eva/dist/jwm-eva.service" "$REPO/eva/dist/"
cat > "$REPO/scripts/ensure_pipewire.sh" <<'STUB'
#!/usr/bin/env bash
printf 'ensure-pipewire %s\n' "$*" >> "$CASE_DIR/eva.log"
printf 'PKG_CONFIG_PATH=%s/pipewire/lib/pkgconfig\nJWM_PIPEWIRE_PREFIX=%s/pipewire\n' "$CASE_DIR" "$CASE_DIR"
STUB
printf 'bundled fixture\n' > "$REPO/assets/wallpapers/01-fixture.png"
touch "$REPO/bars/tao_glow_bar/Cargo.toml" "$REPO/bridge/Cargo.toml" "$REPO/bridge/dist/org.freedesktop.Notifications.service"
touch "$REPO/bars/tao_pixels_bar/Cargo.toml" "$REPO/bars/tauri_leptos_bar/Cargo.toml" "$REPO/crates/jwm_shell/Cargo.toml"
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
if [[ $package == jwm-eva ]]; then
    printf 'eva-build-env <%s> <%s>\n' "${PKG_CONFIG_PATH:-}" "${JWM_PIPEWIRE_PREFIX:-}" >> "$CASE_DIR/eva.log"
    if [[ $SCENARIO == eva-build-failure ]]; then exit 101; fi
fi
if [[ $package == jwm && $SCENARIO =~ ^(missing|repeated|ordinary|tee-cargo)$ ]]; then
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
for binary in jwm jwm-tool jwm-support jwm-remote jwm-bridge jwm-player jwm-shell jwm-eva; do
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
# System paths are logging-only; user files remain within the isolated fixture.
for argument in "$@"; do
    case "$argument" in /usr/*|/etc/*|/opt/*) exit 0 ;; esac
done
/usr/bin/install "$@"
STUB
cat > "$TEST_ROOT/bin/pkg-config" <<'STUB'
#!/usr/bin/env bash
if [[ $SCENARIO == eva-deps-failure || $SCENARIO == eva-skip ]]; then exit 1; fi
if [[ $SCENARIO == tauri-webkit-old ]]; then
    case "$*" in
        '--atleast-version=2.52.6 webkit2gtk-4.1') exit 1 ;;
        '--modversion webkit2gtk-4.1') printf '2.52.3\n' ;;
    esac
fi
exit 0
STUB
cat > "$TEST_ROOT/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CASE_DIR/eva-systemctl.log"
if [[ $SCENARIO == eva-no-bus && $* == *' show '* ]]; then exit 1; fi
if [[ $SCENARIO == eva-no-graphical && $* == *' is-active '* ]]; then exit 1; fi
if [[ $SCENARIO == eva-service-failure && $* == *' restart '* ]]; then exit 69; fi
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
elif [[ ${SCENARIO:-} == sudo-target && $* == '-u fixture-user' ]]; then
    printf '1000\n'
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
    shift 4
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
    printf '# Keep user voice settings\n[interaction]\nfeedback = "text"\n' > "$CASE_DIR/config/jwm/eva.toml"
    cp "$CASE_DIR/config/jwm/eva.toml" "$CASE_DIR/original-eva.toml"
    TMPDIR="$CASE_DIR" SUDO_USER="$sudo_user" XDG_DATA_HOME="$data_home" PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config" CARGO_TARGET_DIR="$target" \
        bash "$REPO/scripts/install-jwm.sh" --mode "$mode" --jobs 2 --bar tao_glow_bar "$@" > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == "$expected" ]] || fail "$scenario/$mode status $status, expected $expected"
    cmp "$CASE_DIR/config/mimeapps.list" "$CASE_DIR/original-mimeapps.list" || fail 'installer changed shared MIME defaults'
    cmp "$CASE_DIR/config/jwm/eva.toml" "$CASE_DIR/original-eva.toml" || fail 'installer changed voice configuration'
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
        grep -Fq '69-jwm-touchpad-gestures.rules /usr/lib/udev/rules.d/69-jwm-touchpad-gestures.rules' "$CASE_DIR/system.log" || fail 'touchpad access rule not installed'
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
        if [[ $scenario == eva-skip ]]; then
            ! grep -Fq '<-p> <jwm-eva>' "$CASE_DIR/cargo.log" || fail '--skip-eva built Eva'
            [[ ! -e $CASE_DIR/eva-systemctl.log ]] || fail '--skip-eva contacted the user service'
        else
            grep -Fq '<-p> <jwm-eva> <--bin> <jwm-eva>' "$CASE_DIR/cargo.log" || fail 'Eva build missing'
            local eva_build
            eva_build=$(grep -F '<-p> <jwm-eva>' "$CASE_DIR/cargo.log")
            [[ $eva_build == *"<--target-dir> <$resolved>"* && $eva_build == *'<-j> <2>'* ]] || fail 'Eva ignored build cache or jobs'
            if [[ $mode == release ]]; then
                [[ $eva_build == *'<--release>'* ]] || fail 'Eva ignored release mode'
            else
                [[ $eva_build != *'<--release>'* ]] || fail 'Eva ignored debug mode'
            fi
            if [[ $scenario == sudo-target ]]; then
                grep -Fq -- "-u fixture-user env HOME=$CASE_DIR/login-home XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus" "$CASE_DIR/system.log" || fail 'Eva did not use the login user session'
                grep -Fq -- "--binary $resolved/$mode/jwm-eva --prefix $CASE_DIR/login-home/.local --config-dir $CASE_DIR/config/jwm --unit-dir $CASE_DIR/config/systemd/user" "$CASE_DIR/system.log" || fail 'sudo Eva install used wrong user or artifact paths'
            else
                [[ -x $CASE_DIR/home/.local/bin/jwm-eva ]] || fail 'Eva user binary missing'
                cmp "$resolved/$mode/jwm-eva" "$CASE_DIR/home/.local/bin/jwm-eva" || fail 'Eva installed the wrong profile artifact'
                grep -Fq -- "--config \"$CASE_DIR/config/jwm/eva.toml\" daemon" "$CASE_DIR/config/systemd/user/jwm-eva.service" || fail 'Eva service uses wrong config'
                grep -Fq "eva-build-env <$CASE_DIR/pipewire/lib/pkgconfig> <$CASE_DIR/pipewire>" "$CASE_DIR/eva.log" || fail 'Eva private runtime environment missing'
                if [[ $scenario == eva-no-bus || $scenario == eva-no-graphical ]]; then
                    ! grep -Fq 'daemon-reload' "$CASE_DIR/eva-systemctl.log" || fail 'no-session install attempted service activation'
                    grep -Fq 'systemctl --user enable --now jwm-eva.service' "$CASE_DIR/output.log" || fail 'no-session install omitted activation instructions'
                else
                    grep -Fq -- '--user restart --no-block jwm-eva.service' "$CASE_DIR/eva-systemctl.log" || fail 'Eva service was not restarted'
                fi
            fi
        fi
        if [[ $scenario == sudo-target ]]; then
            grep -Fq -- "-u fixture-user bash $REPO/scripts/install-wallpapers.sh --dest $CASE_DIR/config/jwm/wallpapers" "$CASE_DIR/system.log" || fail 'sudo wallpaper install did not switch user'
        else
            [[ $(cat "$CASE_DIR/config/jwm/wallpapers/01-fixture.png") == "bundled fixture" ]] || fail 'bundled wallpaper install missing'
        fi
    else
        if [[ $scenario != bridge-failure && $scenario != install-failure && $scenario != player-missing && $scenario != player-install-failure && $scenario != eva-* ]]; then
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
run_case eva-no-bus release 'Eva no session target' 0
run_case eva-no-graphical debug 'Eva no graphical session target' 0
run_case eva-skip debug 'Eva skipped target' 0 --skip-eva
run_case eva-build-failure debug 'Eva build failed target' 101
run_case eva-deps-failure release 'Eva missing headers target' 1
run_case eva-service-failure release 'Eva service failed target' 69
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
        bash "$REPO/scripts/install-jwm.sh" --skip-jwm "$@" > "$CASE_DIR/output.log" 2>&1
    [[ $(grep -c '^install ' "$CASE_DIR/cargo.log") == 1 ]] || fail "$name installed more than one bar"
    grep -Fq -- "--path> <$REPO/bars/$expected>" "$CASE_DIR/cargo.log" || fail "$name selected the wrong bar"
    local expected_config=custom_bar
    [[ $# == 0 ]] || expected_config=$expected
    grep -Fq "\"$expected_config\"" "$CASE_DIR/config/jwm/config_x11.toml" || fail "$name changed or failed to change the selected bar"
    grep -Fq "\"$expected_config\"" "$CASE_DIR/config/jwm/config_wayland.toml" || fail "$name ignored the Wayland bar configuration"
    [[ ! -e $CASE_DIR/eva-systemctl.log && ! -e $CASE_DIR/eva.log ]] || fail '--skip-jwm touched Eva'
}
check_bar_selection default '' tauri_leptos_bar
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
    bash "$REPO/scripts/install-jwm.sh" --skip-jwm --skip-bar > "$CASE_DIR/output.log" 2>&1
for binary in jwm jwm-tool jwm-support jwm-remote jwm-player; do
    [[ $(cat "$CASE_DIR/home/.cargo/bin/$binary") == "old $binary" ]] || fail "--skip-jwm removed $binary"
done
# Reject malformed input before any build, installation or config generation.
check_invalid_args() {
    local status=0
    export CASE_DIR="$TEST_ROOT/invalid-$invalid_index" SCENARIO=success
    mkdir -p "$CASE_DIR/home"
    PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" \
        bash "$REPO/scripts/install-jwm.sh" "$@" > "$CASE_DIR/output.log" 2>&1 || status=$?
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

# Exercise the default Tauri frontend hook, which parses NO_COLOR differently
# from Cargo and rejects conventional non-boolean values such as "1".
mkdir -p "$REPO/bars/tauri_leptos_bar/src-tauri"
touch "$REPO/bars/tauri_leptos_bar/src-tauri/Cargo.toml" \
    "$REPO/bars/tauri_leptos_bar/src-tauri/tauri.conf.json" \
    "$REPO/bars/tauri_leptos_bar/Trunk.toml"
cat > "$TEST_ROOT/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ $* == 'tauri --version' ]]; then
    printf 'tauri-cli 2.12.4\n'
    exit 0
fi
[[ $* == 'tauri build --no-bundle -- --locked' ]] || exit 90
trunk build --release --locked
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\nexit 0\n' > "$CARGO_TARGET_DIR/release/tauri_leptos_bar"
chmod +x "$CARGO_TARGET_DIR/release/tauri_leptos_bar"
STUB
cat > "$TEST_ROOT/bin/trunk" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "${NO_COLOR-unset}" > "$CASE_DIR/frontend-color"
case ${NO_COLOR-unset} in
    true|false|unset) ;;
    *) echo "invalid NO_COLOR boolean: $NO_COLOR" >&2; exit 2 ;;
esac
[[ $SCENARIO != tauri-failure ]] || exit 42
STUB
cat > "$TEST_ROOT/bin/rustup" <<'STUB'
#!/usr/bin/env bash
printf 'wasm32-unknown-unknown\n'
STUB
chmod +x "$TEST_ROOT/bin/trunk" "$TEST_ROOT/bin/rustup"
check_tauri_color() (
    local input=$1 expected=$2 scenario=${3:-success} status=0
    export CASE_DIR="$TEST_ROOT/tauri-$input-$scenario" SCENARIO=$scenario
    mkdir -p "$CASE_DIR/home/.cargo" "$CASE_DIR/config/jwm"
    printf '[status_bar]\nname = "custom_bar"\n' > "$CASE_DIR/config/jwm/config_x11.toml"
    cp "$CASE_DIR/config/jwm/config_x11.toml" "$CASE_DIR/config/jwm/config_wayland.toml"
    if [[ $input == unset ]]; then
        unset NO_COLOR
    else
        export NO_COLOR="$input"
    fi
    JWM_BAR_NAME='' PATH="$TEST_ROOT/bin:$PATH" HOME="$CASE_DIR/home" \
        CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config" \
        bash "$REPO/scripts/install-jwm.sh" --skip-jwm > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $(cat "$CASE_DIR/frontend-color") == "$expected" ]] || fail "Tauri NO_COLOR=$input was not normalized"
    if [[ $scenario == tauri-failure ]]; then
        [[ $status == 42 && ! -e $CASE_DIR/system.log ]] || fail 'Tauri frontend failure was hidden or installed files'
    else
        [[ $status == 0 && -x $CASE_DIR/home/.cargo/bin/tauri_leptos_bar ]] || fail "Tauri NO_COLOR=$input failed to install"
    fi
    [[ ${NO_COLOR-unset} == "$input" ]] || fail 'installer changed caller color preference'
)
check_tauri_color 1 true
check_tauri_color yes true
check_tauri_color true true
check_tauri_color false false
check_tauri_color '' false
check_tauri_color unset unset
check_tauri_color 1 true tauri-failure

# A bar upgrade must keep selecting a previously installed private runtime.
check_tauri_private_runtime() (
    local CASE_DIR="$TEST_ROOT/tauri private runtime" SCENARIO=success
    export CASE_DIR SCENARIO
    export HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config"
    export PATH="$TEST_ROOT/bin:$PATH"
    unset NO_COLOR WEBKIT_INSTALL_PREFIX
    prefix="$HOME/.local/opt/jwm-webkitgtk"
    mkdir -p "$prefix/lib" "$CARGO_HOME/bin" "$XDG_CONFIG_HOME/jwm"
    touch "$prefix/lib/libwebkit2gtk-4.1.so.0" "$prefix/lib/libjavascriptcoregtk-4.1.so.0"
    destination="$CARGO_HOME/bin/tauri_leptos_bar"
    printf '#!/bin/sh\nprintf "original bar\\n"\n' > "$destination"
    chmod +x "$destination"
    for generation in 1 2; do
        JWM_BAR_NAME='' bash "$REPO/scripts/install-jwm.sh" --skip-jwm --skip-eva > "$CASE_DIR/install-$generation.log" 2>&1
        grep -Fq JWM_PRIVATE_WEBKIT_LAUNCHER "$destination" || fail 'Tauri upgrade dropped private runtime launcher'
        [[ -x $prefix/libexec/jwm/tauri_leptos_bar ]] || fail 'private bar executable missing'
        [[ -x $prefix/bin/run-with-webkit-transparent.sh ]] || fail 'launcher depends on the source checkout'
        "$destination" 'argument with spaces'
        [[ $generation != 1 ]] || printf '#!/bin/sh\nexit 49\n' > "$prefix/libexec/jwm/tauri_leptos_bar"
    done
    grep -Fq 'original bar' "$destination.before-webkit" || fail 'original executable backup was overwritten'
    cat > "$prefix/libexec/jwm/tauri_leptos_bar" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$LD_LIBRARY_PATH" "$@"
STUB
    # Extracted distro packages must use matching helpers at their compiled
    # absolute path, while preserving the caller's library path and arguments.
    mkdir -p "$prefix/usr/lib/fixture/webkit2gtk-4.1"
    for process in WebKitWebProcess WebKitNetworkProcess; do
        touch "$prefix/usr/lib/fixture/webkit2gtk-4.1/$process"
        chmod +x "$prefix/usr/lib/fixture/webkit2gtk-4.1/$process"
    done
    cat > "$TEST_ROOT/bin/bwrap" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == --die-with-parent && $2 == --dev-bind && $3 == / && $4 == / &&
   $5 == --ro-bind && $6 == "$WEBKIT_INSTALL_PREFIX/usr/lib/fixture/webkit2gtk-4.1" &&
   $7 == /usr/lib/fixture/webkit2gtk-4.1 && $8 == -- ]] || exit 91
shift 8
exec "$@"
STUB
    chmod +x "$TEST_ROOT/bin/bwrap"
    LD_LIBRARY_PATH=/inherited "$destination" 'argument with spaces' "\$HOME" '' > "$CASE_DIR/launch-output"
    mapfile -t forwarded < "$CASE_DIR/launch-output"
    [[ ${forwarded[0]} == "$prefix/lib:/inherited" && ${forwarded[1]} == 'argument with spaces' &&
       ${forwarded[2]} == "\$HOME" && ${#forwarded[@]} == 4 && -z ${forwarded[3]} ]] || fail 'private runtime changed library path or arguments'
    if WEBKIT_INSTALL_PREFIX="$prefix" bash "$REPO/scripts/install-webkit-bar-launcher.sh" "$destination" "$destination" 2>/dev/null; then
        fail 'launcher was accepted as the real bar executable'
    fi
)
check_tauri_private_runtime

check_tauri_old_webkit() (
    local bar=$1 CASE_DIR="$TEST_ROOT/old-webkit-$1" SCENARIO=tauri-webkit-old status=0
    export CASE_DIR SCENARIO
    mkdir -p "$REPO/bars/$bar/src-tauri" "$CASE_DIR/home/.cargo" "$CASE_DIR/config/jwm"
    touch "$REPO/bars/$bar/src-tauri/Cargo.toml" "$REPO/bars/$bar/src-tauri/tauri.conf.json"
    HOME="$CASE_DIR/home" CARGO_HOME="$CASE_DIR/home/.cargo" XDG_CONFIG_HOME="$CASE_DIR/config" \
        PATH="$TEST_ROOT/bin:$PATH" bash "$REPO/scripts/install-jwm.sh" --bar "$bar" --skip-jwm --skip-eva \
        > "$CASE_DIR/output.log" 2>&1 || status=$?
    [[ $status == 1 ]] || fail "$bar accepted old WebKitGTK"
    grep -Fq 'WebKitGTK >= 2.52.6' "$CASE_DIR/output.log" || fail "$bar omitted required WebKitGTK version"
    grep -Fq 'detected: 2.52.3' "$CASE_DIR/output.log" || fail "$bar omitted detected WebKitGTK version"
    [[ ! -e $CASE_DIR/frontend-color && ! -e $CASE_DIR/home/.cargo/bin/$bar ]] || fail "$bar built or installed despite old WebKitGTK"
)
for host in leptos react solid svelte vue yew; do
    check_tauri_old_webkit "tauri_${host}_bar"
done
printf 'test-install-jwm-build: PASS\n'
